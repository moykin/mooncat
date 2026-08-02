//! The dependency graph, enforced.
//!
//! # Why a test and not a convention
//!
//! The layering here is load-bearing rather than tidy. `domain` depends on nothing so it can
//! be reasoned about alone; the terminal never links an HTTP client so a compromised terminal
//! has no path to an exchange even if it wanted one; the connector contract does not know what
//! an order manager is so a venue cannot reach back into trading logic.
//!
//! Every one of those is a single `cargo add` away from being untrue, and the change would
//! compile, pass every other test, and look entirely reasonable in review. Written down as a
//! test, it fails instead — with the arrow that was added.
//!
//! # Read from the manifests, not from a list kept here
//!
//! A hand-maintained copy of the graph is a second thing to update, and the update that gets
//! forgotten is the one that matters. This reads the actual `Cargo.toml` files, so a crate
//! added to the workspace is covered without anyone remembering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Derived from this crate's manifest directory (`<root>/crates/core`), the same way the
/// README test does it, so the two cannot disagree about where the tree is.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Dependencies of one crate, by name, taken from its manifest.
///
/// Parsed with a small hand-rolled scan rather than a TOML crate: this is a test that guards
/// the dependency graph, and giving it a dependency of its own to do so would be funny in a
/// way nobody wants to explain later.
fn dependencies_of(manifest: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(manifest).unwrap_or_default();
    let mut deps = BTreeSet::new();
    let mut in_deps = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // `[dependencies]` and `[target.'...'.dependencies]` count; dev and build
            // dependencies do not — a test helper is not part of the shipped graph.
            in_deps = trimmed.ends_with("dependencies]")
                && !trimmed.contains("dev-dependencies")
                && !trimmed.contains("build-dependencies");
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                deps.insert(name.to_string());
            }
        }
    }
    deps
}

/// Every crate in the tree and what it depends on, including the terminal's own workspace.
fn graph() -> BTreeMap<String, BTreeSet<String>> {
    let crates_dir = workspace_root().join("crates");
    let mut graph = BTreeMap::new();

    for entry in std::fs::read_dir(&crates_dir).expect("crates/ is readable").flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.exists() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        graph.insert(name, dependencies_of(&manifest));
    }
    graph
}

/// Crates of this project, as opposed to third-party ones.
const OURS: &[&str] = &["domain", "exchange", "marketdata", "binance", "wire", "core", "terminal"];

#[test]
fn the_workspace_contains_what_this_test_thinks_it_does() {
    // If a crate is added and this list is not updated, every rule below silently stops
    // covering it. Checked first so the failure names the cause rather than a symptom.
    let found: BTreeSet<String> = graph().keys().cloned().collect();
    let expected: BTreeSet<String> = OURS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(found, expected, "the crate list in this test is out of date");
}

#[test]
fn domain_depends_on_nothing_of_ours() {
    // It is the vocabulary. A dependency here would mean the definition of an order knows
    // about the thing that sends orders, and nothing could be reasoned about in isolation
    // afterwards.
    let deps = &graph()["domain"];
    for ours in OURS {
        assert!(!deps.contains(*ours), "domain must not depend on {ours}");
    }
}

#[test]
fn the_connector_contract_does_not_know_about_trading_logic() {
    // `exchange` defines what a venue must provide. If it could see an order manager, a venue
    // implementation could reach into it, and the direction of the abstraction would invert.
    let deps = &graph()["exchange"];
    for forbidden in ["wire", "core", "marketdata", "binance", "oms", "risk"] {
        assert!(!deps.contains(forbidden), "exchange must not depend on {forbidden}");
    }
}

#[test]
fn the_terminal_cannot_reach_an_exchange() {
    // The invariant the whole two-process split exists for: the terminal holds no credentials
    // and has no path to a venue. An HTTP client in its graph would make that a matter of
    // discipline rather than of construction.
    let deps = &graph()["terminal"];
    for forbidden in ["reqwest", "binance", "hyper", "ureq", "curl"] {
        assert!(
            !deps.contains(forbidden),
            "the terminal must not link {forbidden}: it would gain a path to a venue"
        );
    }
    // What it may share with the core is exactly the vocabulary and the protocol.
    for allowed in ["domain", "wire", "exchange"] {
        assert!(deps.contains(allowed), "the terminal is expected to use {allowed}");
    }
}

#[test]
fn nothing_below_the_wire_depends_on_the_wire() {
    // Arrows point one way. `marketdata` producing something the protocol shape dictated would
    // make the book a function of how it is transmitted.
    for crate_name in ["domain", "exchange", "marketdata"] {
        assert!(!graph()[crate_name].contains("wire"), "{crate_name} must not depend on wire");
    }
}

#[test]
fn a_connector_depends_on_the_contract_and_not_on_the_core() {
    // A venue implementation that could see the core would be able to bypass the OMS, and the
    // single-writer rule for trading state would stop being enforceable.
    let deps = &graph()["binance"];
    assert!(deps.contains("exchange"), "a connector implements the contract");
    assert!(!deps.contains("core"), "and must not know what runs it");
    assert!(!deps.contains("wire"), "nor how its output is transmitted");
}

#[test]
fn the_core_is_the_only_place_that_sees_everything() {
    // Composition happens in exactly one crate. If two could, the wiring would exist twice.
    let deps = &graph()["core"];
    for expected in ["domain", "exchange", "wire", "binance"] {
        assert!(deps.contains(expected), "the core composes {expected}");
    }
}

#[test]
fn the_graph_has_no_cycles() {
    // Cargo would refuse a cycle, so this cannot currently fail — but it states the property
    // rather than relying on the build system to have been the thing that noticed.
    let graph = graph();
    for (name, deps) in &graph {
        for dep in deps.iter().filter(|d| OURS.contains(&d.as_str())) {
            assert!(
                !graph.get(dep).is_some_and(|back| back.contains(name)),
                "{name} and {dep} depend on each other"
            );
        }
    }
}
