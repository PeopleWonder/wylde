//! Windowed gpui tests for the concept-routing **R3b** dependency-tree view
//! (concept-routing plan §5, relation-model addendum §4.3).
//!
//! Mount the real views in a gpui test window, drive them through the scripted
//! fake backend at the `wylde_gui_pipe::call` chokepoint, and assert on
//! observable state — no live `wylde-workspaces` stack. See `docs/gui-testing.md`.
//!
//! What they cover:
//!   (a) the tree view loads the relation graph + builds the typed-edge model;
//!   (b) the Relations editor toggles to the tree (mounted beside it) + back;
//!   (c) a node selection in the tree deep-links the editor onto that node
//!       (reusing `set_focus`) and flips back to the editor.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::routing::ipc::NodeRefView;
use wylde_panel_workspaces::routing::tree_view::{DependencyTreeView, TreeEvent};
use wylde_panel_workspaces::routing::RelationsView;

/// A `workspaces.concepts.search` reply with two concepts.
fn concepts_reply() -> Value {
    json!({
        "workspace_id": "ws-a",
        "results": [
            { "concept": { "id": "nextcloud", "label": "Nextcloud", "description": "",
                "members": [], "member_files": [], "parent_concepts": [], "source": "manual" },
              "score": 0.0, "fuzzy": 0.0, "semantic": 0.0 },
            { "concept": { "id": "wylde", "label": "Wylde", "description": "",
                "members": [], "member_files": [], "parent_concepts": [], "source": "manual" },
              "score": 0.0, "fuzzy": 0.0, "semantic": 0.0 }
        ]
    })
}

fn anchors_reply() -> Value {
    json!({
        "anchors": [{
            "identifier": "ddns", "kind": "concept",
            "target": { "type": "concept", "text": "dynamic DNS" },
            "description": "keeps the home IP current"
        }]
    })
}

/// `relations.graph`: Nextcloud depends-on DDNS (hierarchy) + Nextcloud IS-NOT
/// Wylde (severed exclusion) — one of each kind the tree must distinguish.
fn graph_reply() -> Value {
    json!({
        "count": 2,
        "relations": [
            { "from": { "node": "concept", "id": "nextcloud" },
              "to": { "node": "vocab", "identifier": "ddns" }, "kind": "dependency" },
            { "from": { "node": "concept", "id": "nextcloud" },
              "to": { "node": "concept", "id": "wylde" }, "kind": "negative" }
        ]
    })
}

fn backend() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "active_id": "ws-a", "workspaces": [{ "id": "ws-a", "folder": "C:/code/a" }] }),
        )
        .on("workspaces.concepts.search", concepts_reply())
        .on("workspaces.anchors.list", anchors_reply())
        .on("workspaces.concepts.relations.graph", graph_reply())
        .on("workspaces.concepts.relations.list", json!({ "count": 0, "relations": [] }))
}

// ── (a) the tree view loads + builds the typed-edge model ─────────────────

#[gpui::test]
fn tree_loads_graph_and_builds_model(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| DependencyTreeView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_loading(), "load resolved");
            assert_eq!(view.workspace_id(), Some("ws-a"));
            // 3 distinct nodes (nextcloud, ddns, wylde) and 2 typed edges.
            assert_eq!(view.node_count(), 3, "all relation nodes projected");
            assert_eq!(view.edge_count(), 2, "dependency + exclusion drawn");
        })
        .unwrap();

    assert!(fake.count_for("workspaces.concepts.relations.graph") >= 1);
}

// ── (b) the editor toggles to the tree and back ───────────────────────────

#[gpui::test]
fn editor_toggles_to_tree_and_back(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, cx| {
            assert!(!view.show_tree(), "starts on the editor");
            view.set_show_tree(true, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |view, _w, cx| {
            assert!(view.show_tree(), "switched to the tree");
            // The child tree built its model from the same graph.
            assert_eq!(view.tree_view().read(cx).node_count(), 3);
            view.set_show_tree(false, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| assert!(!view.show_tree(), "back to editor"))
        .unwrap();
}

// ── (c) a tree selection deep-links the editor ────────────────────────────

#[gpui::test]
fn tree_selection_deep_links_the_editor(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    // Show the tree, then emit a node selection (what a click produces) from
    // the child tree entity — the editor's subscription must focus that node.
    window
        .update(cx, |view, _w, cx| {
            view.set_show_tree(true, cx);
            let tree = view.tree_view().clone();
            tree.update(cx, |_t, tcx| {
                tcx.emit(TreeEvent::Selected(NodeRefView::concept("nextcloud")));
            });
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.show_tree(), "selection flips back to the editor");
            assert_eq!(
                view.focus(),
                Some(&NodeRefView::concept("nextcloud")),
                "the editor deep-linked onto the clicked node"
            );
        })
        .unwrap();
}
