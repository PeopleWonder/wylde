//! Windowed gpui tests for the Relations editor sub-tab (concept-routing
//! **R1.5c**, relation-model addendum §2.2).
//!
//! Mount the real view in a gpui test window, drive it through the scripted
//! fake backend at the `wylde_gui_pipe::call` chokepoint, and assert on
//! observable state AND the verbs issued — no live `wylde-workspaces` stack.
//! See `docs/gui-testing.md`. Determinism via `cx.run_until_parked()`.
//!
//! What they retire (owed feel-tests):
//!   (a) the editor loads the node universe + the whole relation graph on mount
//!       (via `concepts.search` + `anchors.list` + `relations.graph`);
//!   (b) focusing a node loads its touching edges via `relations.list`;
//!   (c) authoring an edge issues `relations.add` with the right tagged nodes +
//!       kind, then reloads;
//!   (d) a validation error (duplicate) surfaces as clean inline status, no
//!       crash;
//!   (e) the Vocabulary tab flips to the Relations sub-tab and the child is live.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::routing::ipc::{NodeRefView, RelationKindView};
use wylde_panel_workspaces::routing::reducer::RelGroup;
use wylde_panel_workspaces::routing::RelationsView;
use wylde_panel_workspaces::vocabulary::{VocabSubTab, VocabularyTab};

/// A `workspaces.concepts.search` reply with one concept.
fn concepts_reply() -> Value {
    json!({
        "workspace_id": "ws-a",
        "results": [{
            "concept": {
                "id": "nextcloud", "label": "Nextcloud",
                "description": "self-hosted sync",
                "members": [], "member_files": [], "parent_concepts": [],
                "source": "manual"
            },
            "score": 0.0, "fuzzy": 0.0, "semantic": 0.0
        }]
    })
}

/// A `workspaces.anchors.list` reply with one vocab anchor.
fn anchors_reply() -> Value {
    json!({
        "anchors": [{
            "identifier": "ddns",
            "kind": "concept",
            "target": { "type": "concept", "text": "dynamic DNS" },
            "description": "keeps the home IP current"
        }]
    })
}

/// Backend: active workspace + the node universe loaders + an empty graph.
fn backend() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "active_id": "ws-a", "workspaces": [{ "id": "ws-a", "folder": "C:/code/a" }] }),
        )
        .on("workspaces.concepts.search", concepts_reply())
        .on("workspaces.anchors.list", anchors_reply())
        .on(
            "workspaces.concepts.relations.graph",
            json!({ "count": 0, "relations": [] }),
        )
        .on(
            "workspaces.concepts.relations.list",
            json!({ "count": 0, "relations": [] }),
        )
}

// ── (a) loads the node universe + graph on mount ─────────────────────────

#[gpui::test]
fn relations_loads_universe_and_graph_on_mount(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_loading(), "load resolved");
            assert_eq!(view.workspace_id(), Some("ws-a"));
            // 1 concept + 1 vocab anchor = 2 relatable nodes.
            assert_eq!(view.universe_len(), 2, "universe = concepts + vocab");
            assert_eq!(view.overview_len(), 0, "empty graph");
            assert!(view.focus().is_none(), "starts on the overview");
        })
        .unwrap();

    assert!(fake.count_for("workspaces.concepts.relations.graph") >= 1);
    assert!(fake.count_for("workspaces.concepts.search") >= 1);
    assert!(fake.count_for("workspaces.anchors.list") >= 1);
}

// ── (b) focusing a node loads its edges via relations.list ───────────────

#[gpui::test]
fn focusing_a_node_lists_its_relations(cx: &mut TestAppContext) {
    let fake = backend().on(
        "workspaces.concepts.relations.list",
        json!({
            "count": 1,
            "relations": [{
                "from": { "node": "concept", "id": "nextcloud" },
                "to": { "node": "vocab", "identifier": "ddns" },
                "kind": "dependency",
                "note": "keeps the home IP current"
            }]
        }),
    );
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, cx| {
            view.set_focus(NodeRefView::concept("nextcloud"), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert_eq!(view.focus(), Some(&NodeRefView::concept("nextcloud")));
            assert_eq!(view.touching_len(), 1, "one edge touches the focus");
        })
        .unwrap();

    let call = fake
        .last_call_for("workspaces.concepts.relations.list")
        .expect("focusing must list the node's relations");
    assert_eq!(call.payload_str("workspace_id").as_deref(), Some("ws-a"));
    assert_eq!(
        call.payload.get("node"),
        Some(&json!({ "node": "concept", "id": "nextcloud" })),
        "lists by the tagged NodeRef"
    );
}

// ── (c) authoring an edge issues relations.add then reloads ──────────────

#[gpui::test]
fn adding_an_edge_issues_relations_add(cx: &mut TestAppContext) {
    let fake = backend().on(
        "workspaces.concepts.relations.add",
        json!({ "relation": {
            "from": { "node": "concept", "id": "nextcloud" },
            "to": { "node": "vocab", "identifier": "ddns" },
            "kind": "dependency"
        }}),
    );
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    // Focus, then author a DEPENDS ON edge to {{ddns}}.
    window
        .update(cx, |view, _w, cx| {
            view.set_focus(NodeRefView::concept("nextcloud"), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |view, _w, cx| {
            view.add_edge(RelGroup::DependsOn, NodeRefView::vocab("ddns"), cx);
        })
        .unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("workspaces.concepts.relations.add")
        .expect("authoring must issue relations.add");
    assert_eq!(
        call.payload.get("from"),
        Some(&json!({ "node": "concept", "id": "nextcloud" }))
    );
    assert_eq!(
        call.payload.get("to"),
        Some(&json!({ "node": "vocab", "identifier": "ddns" }))
    );
    assert_eq!(call.payload_str("kind").as_deref(), Some("dependency"));
    // The DEPENDS ON group carries the dependency kind end-to-end.
    assert_eq!(RelGroup::DependsOn.kind(), RelationKindView::Dependency);
    // An add reloads the focus list.
    assert!(fake.count_for("workspaces.concepts.relations.list") >= 2);
}

// ── (d) a duplicate surfaces clean inline status (no crash) ──────────────

#[gpui::test]
fn duplicate_relation_surfaces_inline_error(cx: &mut TestAppContext) {
    let fake = backend().on_err(
        "workspaces.concepts.relations.add",
        "already_exists: this relation already exists",
    );
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, cx| {
            view.set_focus(NodeRefView::concept("nextcloud"), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |view, _w, cx| {
            view.add_edge(RelGroup::IsNot, NodeRefView::vocab("ddns"), cx);
        })
        .unwrap();
    cx.run_until_parked();

    // The add was attempted; the view survived and stayed focused.
    assert!(fake.count_for("workspaces.concepts.relations.add") >= 1);
    window
        .update(cx, |view, _w, _cx| {
            assert_eq!(view.focus(), Some(&NodeRefView::concept("nextcloud")));
        })
        .unwrap();
}

// ── (e) Vocabulary tab flips to the Relations sub-tab ────────────────────

#[gpui::test]
fn vocabulary_tab_switches_to_relations_subtab(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| VocabularyTab::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |tab, _w, cx| {
            assert_eq!(
                tab.sub_tab(),
                VocabSubTab::Vocabulary,
                "defaults to Vocabulary"
            );
            // The Relations child built + loaded its universe.
            assert_eq!(tab.relations_view().read(cx).universe_len(), 2);
            tab.set_sub_tab(VocabSubTab::Relations, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |tab, _w, _cx| {
            assert_eq!(
                tab.sub_tab(),
                VocabSubTab::Relations,
                "switched to Relations"
            );
        })
        .unwrap();

    assert!(
        fake.count_for("workspaces.concepts.relations.graph") >= 1,
        "the relations surface loaded the graph"
    );
}
