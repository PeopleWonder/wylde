//! Structural validity of `docs/first-run-bootstrap.md`.
//!
//! The bootstrap doc is the playbook the on-device LLM follows the
//! very first time Wylde boots. Every tool id / pipe action / broker
//! action it cites must resolve in the live registry; a typo or a
//! stale id would silently break the bootstrap with no easy way to
//! diagnose. This test parses the doc's appendix (the explicit
//! "Tool / action reference" section) and asserts every cited id
//! actually exists.
//!
//! ## Why an appendix instead of grepping the whole doc
//!
//! Free-text mentions in the doc body would force every reference to
//! be exact and would generate false positives on identifiers used
//! conversationally ("call `consent.set`" vs. "the consent.* surface"
//! vs. "decision is a string"). The appendix is the contract; the
//! body is the explanation. The bootstrap LLM and any human reader
//! both check the appendix when in doubt.
//!
//! ## What counts as "resolves"
//!
//! * Pipe verbs (`service.verb` shape): must appear in either
//!   `wylde_harness::pipe::ALL_PIPE_ACTIONS` or the known broker
//!   action whitelist (`vram.*`, `system.*`) maintained below.
//! * Tool ids (snake_case shape): must resolve in the global tool
//!   registry via [`Registry::lookup`].

#![cfg(windows)]

use std::collections::HashSet;
use std::path::PathBuf;

use wylde_harness::pipe::ALL_PIPE_ACTIONS;
use wylde_harness::tooling::registry::global;

/// Broker actions are not part of the harness pipe surface, so they
/// don't appear in ALL_PIPE_ACTIONS. They live on the
/// `wylde-vram-broker` service. Kept here as a small whitelist —
/// the bootstrap doc only mentions a couple, and gating them this
/// way means a new broker action has to be deliberately added to
/// the whitelist before the doc may cite it.
const BROKER_ACTIONS: &[&str] = &["system.inventory"];

/// Tools the doc explicitly calls out as "deferred" so the LLM
/// knows the dispatch will return a `phase_<n>_deferred` error.
/// Listed here so future deferred-mentions don't quietly drop off.
const KNOWN_DEFERRED_REFERENCES: &[&str] = &["memory_workspace_save"];

fn bootstrap_doc_path() -> PathBuf {
    // Walk up from CARGO_MANIFEST_DIR (`rust/crates/wylde-harness`)
    // to the wylde root (`./../../..`), then into `docs/`.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(3)
        .expect("walk up 3 levels from manifest")
        .join("docs")
        .join("first-run-bootstrap.md")
}

fn read_appendix() -> String {
    let path = bootstrap_doc_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // The appendix is everything after the "Tool / action reference"
    // heading.
    let heading_idx = text
        .find("## Tool / action reference")
        .expect("doc must contain the Tool / action reference appendix");
    text[heading_idx..].to_string()
}

/// Extract every backtick-quoted token from list-item lines in the
/// appendix. We deliberately ignore in-prose mentions ("the `name`
/// field of the payload") because those are JSON-field references,
/// not tool ids — counting them would generate false positives.
/// Lines that start with `-` (after optional whitespace) are the
/// contract; the bullet form is what the doc commits to.
fn extract_ids(appendix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in appendix.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("- ") {
            continue;
        }
        let mut buf = String::new();
        let mut in_tick = false;
        for ch in trimmed.chars() {
            if ch == '`' {
                if in_tick {
                    let candidate = buf.trim().to_string();
                    buf.clear();
                    if looks_like_id(&candidate) {
                        out.push(candidate);
                    }
                }
                in_tick = !in_tick;
                continue;
            }
            if in_tick {
                buf.push(ch);
            }
        }
    }
    out
}

fn looks_like_id(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap();
    if !first.is_ascii_lowercase() {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

#[test]
fn every_id_in_bootstrap_appendix_resolves_somewhere() {
    let appendix = read_appendix();
    let ids = extract_ids(&appendix);

    assert!(!ids.is_empty(), "appendix produced no parseable ids");

    let pipe_actions: HashSet<&str> = ALL_PIPE_ACTIONS.iter().copied().collect();
    let broker_actions: HashSet<&str> = BROKER_ACTIONS.iter().copied().collect();
    let registry = global();

    let mut unresolved: Vec<String> = Vec::new();
    for id in &ids {
        // Pipe action shape: contains a dot.
        if id.contains('.') {
            if pipe_actions.contains(id.as_str()) || broker_actions.contains(id.as_str()) {
                continue;
            }
            unresolved.push(format!("{id} (pipe-shape, not in pipe + broker action sets)"));
            continue;
        }
        // Tool-id shape: snake_case, no dots.
        if registry.lookup(id).is_some() {
            continue;
        }
        unresolved.push(format!("{id} (no registry entry)"));
    }

    assert!(
        unresolved.is_empty(),
        "bootstrap doc cites ids that do not resolve:\n  - {}",
        unresolved.join("\n  - ")
    );

    // Cross-check: at least one deferred reference made it through —
    // confirms the registry's lookup follows the deferred path too.
    for d in KNOWN_DEFERRED_REFERENCES {
        assert!(
            ids.iter().any(|id| id == d),
            "expected appendix to mention deferred id {d}; got: {ids:?}"
        );
    }
}

#[test]
fn appendix_lists_core_pipe_verbs_for_each_subsystem() {
    // The doc is no good if it forgets to mention how to list tools,
    // run them, or respond to consent prompts. Pin those as
    // must-mention.
    let appendix = read_appendix();
    let ids: HashSet<String> = extract_ids(&appendix).into_iter().collect();
    for required in [
        "system.inventory",
        "tools.list",
        "tools.run",
        "consent.list",
        "consent.respond",
        "memory.long_term.save",
        "memory.workspaces.list",
    ] {
        assert!(
            ids.contains(required),
            "bootstrap doc appendix is missing required id `{required}`"
        );
    }
}

#[test]
fn no_typoed_consent_verbs_in_appendix() {
    // Catches the common rename trap: someone adds `consent.approve`
    // to the doc body, never wires it as a pipe action, and
    // bootstrap LLMs silently get `no_action` errors. The set of
    // consent.* ids in the appendix MUST be a subset of the live
    // ALL_PIPE_ACTIONS surface.
    let appendix = read_appendix();
    let ids = extract_ids(&appendix);
    let consent_in_doc: HashSet<&str> = ids
        .iter()
        .filter(|id| id.starts_with("consent."))
        .map(|s| s.as_str())
        .collect();
    let consent_in_pipe: HashSet<&str> = ALL_PIPE_ACTIONS
        .iter()
        .copied()
        .filter(|n| n.starts_with("consent."))
        .collect();
    let extras: Vec<&&str> = consent_in_doc.difference(&consent_in_pipe).collect();
    assert!(
        extras.is_empty(),
        "appendix references consent.* verbs missing from pipe: {extras:?}"
    );
}
