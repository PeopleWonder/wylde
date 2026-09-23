//! Guard: the `panel-walk` alias's `-p` list covers every panel crate (#95).
//!
//! ## Why this exists
//!
//! `Core/GUI/.cargo/config.toml` defines the required L7 gate as a hand-kept
//! crate list:
//!
//! ```text
//! panel-walk = "test -p wylde-panel-chat -p wylde-panel-dashboard … -p wylde-panel-settings"
//! ```
//!
//! The `gui panel-walk (L7)` required CI job runs exactly this. Add a tenth
//! panel crate (a new `Frontend/Panels/*` workspace member with its own
//! `tests/panel_walk.rs`) and forget to extend the alias, and the gate
//! **silently never runs that panel's tests** — CI stays green, nothing signals
//! the new panel is unguarded. That is the #83 "looks-armed-isn't" family: a
//! required check structurally capable of passing while covering less than it
//! appears to.
//!
//! `cargo test --workspace` auto-discovers members and would pick a new panel up
//! — but the alias is deliberately `-p`-scoped so the headless gate never links
//! the Shell's `wry`/tray-icon graph (config comment). This test buys back the
//! drift-robustness the scoping costs: it asserts the alias's `-p` set ⊇ every
//! panel member declared in `Cargo.toml`, turning silent under-coverage into a
//! red that names the missing `-p`.
//!
//! It lives here (a crate already in the alias) so it always runs under the L7
//! gate regardless of which panel is the one omitted.

use std::fs;
use std::path::{Path, PathBuf};

/// The `Core/GUI` root, derived from this crate's manifest dir
/// (`Core/GUI/Frontend/Panels/Workspaces`) so it survives a checkout anywhere.
/// Same idiom as `fixture_pipes_are_private.rs`.
fn gui_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("manifest dir should be Core/GUI/Frontend/Panels/Workspaces")
        .to_path_buf()
}

/// Extract the double-quoted strings inside the `[workspace] members = [ … ]`
/// array of `Core/GUI/Cargo.toml`. Line-based on purpose — no toml dep.
fn workspace_members(cargo_toml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_members = false;
    for line in cargo_toml.lines() {
        let t = line.trim();
        if t.starts_with("members") && t.contains('[') {
            in_members = true;
            continue;
        }
        if in_members {
            if t.starts_with(']') {
                break;
            }
            if let Some(start) = t.find('"') {
                if let Some(end) = t[start + 1..].find('"') {
                    out.push(t[start + 1..start + 1 + end].to_string());
                }
            }
        }
    }
    out
}

/// Read `[package] name` from a member's `Cargo.toml`.
fn crate_name(member_dir: &Path) -> String {
    let manifest = member_dir.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("name") {
            let rest = rest.trim_start_matches([' ', '=']);
            if let Some(start) = rest.find('"') {
                if let Some(end) = rest[start + 1..].find('"') {
                    return rest[start + 1..start + 1 + end].to_string();
                }
            }
        }
    }
    panic!("no [package] name in {}", manifest.display());
}

/// The set of crate names passed to the `panel-walk` alias via `-p`.
fn panel_walk_dash_p(config_toml: &str) -> Vec<String> {
    let line = config_toml
        .lines()
        .find(|l| l.trim_start().starts_with("panel-walk"))
        .expect("no `panel-walk` alias in Core/GUI/.cargo/config.toml");
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut out = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        if *tok == "-p" {
            if let Some(name) = tokens.get(i + 1) {
                out.push(name.trim_matches('"').to_string());
            }
        }
    }
    out
}

#[test]
fn panel_walk_alias_covers_every_panel_crate() {
    let root = gui_root();
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read Core/GUI/Cargo.toml");
    let config_toml =
        fs::read_to_string(root.join(".cargo/config.toml")).expect("read .cargo/config.toml");

    // Panel members: `Frontend/Panels/*`, excluding the `shared/*` helper crates
    // (not panels — no `tests/panel_walk.rs`, and deliberately not in the alias).
    let panel_members: Vec<String> = workspace_members(&cargo_toml)
        .into_iter()
        .filter(|m| m.starts_with("Frontend/Panels/") && !m.starts_with("Frontend/Panels/shared/"))
        .collect();
    assert!(
        !panel_members.is_empty(),
        "parsed zero panel members from Core/GUI/Cargo.toml — the parser or the layout changed"
    );

    let covered = panel_walk_dash_p(&config_toml);

    let missing: Vec<String> = panel_members
        .iter()
        .map(|m| crate_name(&root.join(m)))
        .filter(|name| !covered.contains(name))
        .collect();

    assert!(
        missing.is_empty(),
        "the `panel-walk` alias in Core/GUI/.cargo/config.toml does not cover these panel \
         crates, so the required `gui panel-walk (L7)` gate silently skips their tests — add \
         `-p <name>` for each: {missing:?}\n(covered: {covered:?})"
    );
}
