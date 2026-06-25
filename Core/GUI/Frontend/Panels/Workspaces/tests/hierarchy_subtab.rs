//! Windowed gpui tests for the Hierarchy sub-tab (definitional-hierarchy H2).
//!
//! Mount the real view in a gpui test window, drive it through the scripted
//! fake backend at the `wylde_gui_pipe::call` chokepoint, and assert on
//! observable state AND the verbs issued — no live `wylde-workspaces` stack.
//! See `docs/gui-testing.md`. Determinism via `cx.run_until_parked()`.
//!
//! What they retire (owed feel-tests):
//!   (a) the tab loads the whole projected+overlaid DAG on mount via get_tree;
//!   (b) master toggle OFF ⇒ the tab is inert (no tree, enabled=false) and the
//!       Enable button issues `set_enabled`;
//!   (c) drill-down: expanding a node + selecting it drives the view state;
//!   (d) the Vocabulary tab flips to the Hierarchy sub-tab and the child is live.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::hierarchy::HierarchyView;
use wylde_panel_workspaces::vocabulary::{VocabSubTab, VocabularyTab};

/// A `get_tree` reply: auth (root) → token (leaf, needs definition).
fn tree_reply(enabled: bool) -> Value {
    if !enabled {
        return json!({ "enabled": false, "nodes": [], "count": 0, "roots": [], "dangling_count": 0 });
    }
    json!({
        "enabled": true,
        "count": 2,
        "roots": ["concept:auth"],
        "leaves": ["concept:token"],
        "dangling_count": 0,
        "nodes": [
            { "id": "concept:auth", "label": "Auth",
              "definition": { "text": "authentication", "source": "inherited_concept" },
              "kind": "concept", "parents": [], "children": ["concept:token"], "is_leaf": false },
            { "id": "concept:token", "label": "Token",
              "definition": { "text": "", "source": "missing" },
              "kind": "concept", "parents": ["concept:auth"], "children": [], "is_leaf": true }
        ]
    })
}

fn backend(enabled: bool) -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "active_id": "ws-a", "workspaces": [{ "id": "ws-a", "folder": "C:/code/a" }] }),
        )
        .on("workspaces.hierarchy.get_tree", tree_reply(enabled))
}

// ── (a) loads the DAG on mount ───────────────────────────────────────────

#[gpui::test]
fn hierarchy_loads_tree_on_mount(cx: &mut TestAppContext) {
    let fake = backend(true);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| HierarchyView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_loading(), "load resolved");
            assert_eq!(view.workspace_id(), Some("ws-a"));
            assert!(view.is_enabled(), "toggle on => tree shown");
            assert_eq!(view.node_count(), 2, "auth + token");
        })
        .unwrap();

    assert!(fake.count_for("workspaces.hierarchy.get_tree") >= 1);
}

// ── (b) master toggle OFF ⇒ inert; Enable issues set_enabled ─────────────

#[gpui::test]
fn toggle_off_is_inert_and_enable_issues_set_enabled(cx: &mut TestAppContext) {
    let fake = backend(false).on("workspaces.hierarchy.set_enabled", json!({ "enabled": true }));
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| HierarchyView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_enabled(), "toggle off");
            assert_eq!(view.node_count(), 0, "no tree when off");
        })
        .unwrap();

    // The Enable button drives set_enabled(true).
    window
        .update(cx, |view, _w, cx| view.set_enabled(true, cx))
        .unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("workspaces.hierarchy.set_enabled")
        .expect("Enable must issue set_enabled");
    assert_eq!(call.payload.get("enabled"), Some(&json!(true)));
}

// ── (c) drill-down: expand + select drive the view state ─────────────────

#[gpui::test]
fn expand_and_select_drive_state(cx: &mut TestAppContext) {
    let fake = backend(true);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| HierarchyView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, cx| {
            assert!(view.selected().is_none(), "nothing selected initially");
            view.expand("concept:auth", cx);
            view.select("concept:token", cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert_eq!(view.selected(), Some("concept:token"));
        })
        .unwrap();
}

// ── (d) Vocabulary tab flips to the Hierarchy sub-tab ────────────────────

#[gpui::test]
fn vocabulary_tab_switches_to_hierarchy_subtab(cx: &mut TestAppContext) {
    let fake = backend(true);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| VocabularyTab::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |tab, _w, cx| {
            assert_eq!(tab.sub_tab(), VocabSubTab::Vocabulary, "defaults to Vocabulary");
            // The Hierarchy child built + loaded its tree on mount.
            assert_eq!(tab.hierarchy_view().read(cx).node_count(), 2);
            tab.set_sub_tab(VocabSubTab::Hierarchy, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |tab, _w, _cx| {
            assert_eq!(tab.sub_tab(), VocabSubTab::Hierarchy, "switched to Hierarchy");
        })
        .unwrap();

    assert!(
        fake.count_for("workspaces.hierarchy.get_tree") >= 1,
        "the hierarchy surface loaded its tree"
    );
}
