#!/usr/bin/env bash
#
# Guards the repository against re-acquiring build output.
#
# This is not hypothetical hygiene. `crates/terminal/target` was tracked for eleven commits
# because `.gitignore` said `/target` — anchored to the root, so it never matched the nested
# workspace. The cost by the time it was noticed: 17 261 tracked files out of 17 308, and a
# 1.8 GiB `.git`. Purging it needed a full history rewrite.
#
# The thresholds are deliberately far above current values (47 files, ~200 KiB pack). They
# catch a category error — build output, vendored binaries, a committed database — not
# ordinary growth. Raise them when the source tree genuinely outgrows them; do not raise
# them to make a red build go green.

set -euo pipefail

MAX_FILES=400
MAX_PACK_MIB=5

fail=0
note() { printf '  %s\n' "$1"; }

# --- Build output must not be tracked, at any depth ------------------------------------

# `git ls-files` lists the index, so this catches a file staged in this very commit — not
# just one that landed earlier.
tracked_target=$(git ls-files | grep -E '(^|/)target/' | head -20 || true)
if [[ -n "$tracked_target" ]]; then
    echo "FAIL: build output is tracked by git"
    note "these are under a target/ directory and must not be in the index:"
    printf '    %s\n' $tracked_target
    note "fix: git rm -r --cached <dir> && ensure .gitignore has 'target/' (no leading slash)"
    fail=1
fi

# --- The leading-slash trap itself -----------------------------------------------------

# `/target` only matches the root. Catching the pattern is better than catching its
# consequences, because the consequences take a history rewrite to undo.
if [[ -f .gitignore ]] && grep -qE '^/target/?$' .gitignore; then
    echo "FAIL: .gitignore uses '/target', which is anchored to the repository root"
    note "a nested workspace such as crates/terminal/target is NOT ignored by it"
    note "fix: replace with 'target/'"
    fail=1
fi

# --- File count ------------------------------------------------------------------------

files=$(git ls-files | wc -l | tr -d ' ')
if (( files > MAX_FILES )); then
    echo "FAIL: $files tracked files exceeds the limit of $MAX_FILES"
    note "largest directories by tracked file count:"
    git ls-files | xargs -n1 dirname | sort | uniq -c | sort -rn | head -5 | sed 's/^/    /'
    fail=1
fi

# --- Pack size -------------------------------------------------------------------------

# size-pack is reported in KiB by `-v`; `-vH` is for humans and awkward to compare against.
pack_kib=$(git count-objects -v | awk '/^size-pack:/ {print $2}')
pack_mib=$(( pack_kib / 1024 ))
if (( pack_mib > MAX_PACK_MIB )); then
    echo "FAIL: pack is ${pack_mib} MiB, over the ${MAX_PACK_MIB} MiB limit"
    note "history carries something it should not; 'git count-objects -vH' has the detail"
    fail=1
fi

if (( fail )); then
    exit 1
fi

echo "repo clean: ${files} tracked files, ${pack_kib} KiB pack, no build output in the index"
