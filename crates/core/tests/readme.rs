//! Keeps the test counts in `README.md` honest.
//!
//! The README claimed 91 and 14 for several commits while the real numbers were 116 and 41.
//! Nobody lied — the counts were written once and never revisited, which is what always
//! happens to a number a human has to remember to update. So the number is checked instead.
//!
//! Counting `#[test]` attributes is an approximation of what `cargo test` reports, and it is
//! exact only while three things hold: no `#[ignore]`, no macro that generates test functions,
//! and no doctests. All three hold today. If one stops holding, this test will fail with the
//! two numbers side by side, which is the right moment to decide what the README should say.

use std::path::{Path, PathBuf};

/// Workspace root, derived from this crate's manifest directory (`<root>/crates/core`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Test functions under a directory tree.
///
/// `#[tokio::test]` expands to `#[test]`, but the source text carries only the former, so both
/// spellings are counted. Attributes inside a line comment would be miscounted; there are none,
/// and a stray one would show up as an off-by-one rather than as silence.
fn count_tests(dir: &Path) -> usize {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_tests(&path);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            total += text
                .lines()
                .map(str::trim)
                .filter(|line| *line == "#[test]" || *line == "#[tokio::test]")
                .count();
        }
    }
    total
}

/// Every crate in a workspace, counting both unit tests (`src/`) and integration tests
/// (`tests/`) — `cargo test` reports the sum of the two.
fn count_workspace(crates_dir: &Path, terminal: bool) -> usize {
    let mut total = 0;
    let entries = std::fs::read_dir(crates_dir).expect("crates/ is readable");
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_terminal = path.file_name().is_some_and(|n| n == "terminal");
        if is_terminal != terminal {
            continue;
        }
        total += count_tests(&path.join("src"));
        total += count_tests(&path.join("tests"));
    }
    total
}

/// Pulls the number out of a `# N tests` style comment in a fenced block.
fn claimed(readme: &str, marker: &str) -> usize {
    let line = readme
        .lines()
        .find(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("README has no line containing `{marker}` — did the Build section move?"));
    let digits: String =
        line.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or_else(|_| panic!("no number on README line: {line}"))
}

#[test]
fn readme_test_counts_match_reality() {
    let root = workspace_root();
    let readme = std::fs::read_to_string(root.join("README.md")).expect("README.md is readable");
    let crates = root.join("crates");

    let main_actual = count_workspace(&crates, false);
    let main_claimed = claimed(&readme, "cargo test --workspace");
    assert_eq!(
        main_claimed, main_actual,
        "README says {main_claimed} tests in the main workspace, the tree has {main_actual}"
    );

    let terminal_actual = count_workspace(&crates, true);
    let terminal_claimed = claimed(&readme, "cd crates/terminal && cargo test");
    assert_eq!(
        terminal_claimed, terminal_actual,
        "README says {terminal_claimed} terminal tests, the tree has {terminal_actual}"
    );
}

/// The guard from task 0.1 is only useful if it is present and runnable.
#[test]
fn the_repo_size_guard_exists_and_is_executable() {
    let guard = workspace_root().join("ci/check-repo-size.sh");
    let meta = std::fs::metadata(&guard).expect("ci/check-repo-size.sh exists");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            meta.permissions().mode() & 0o111 != 0,
            "ci/check-repo-size.sh is not executable — CI would run it as a no-op"
        );
    }
    #[cfg(not(unix))]
    let _ = meta;
}
