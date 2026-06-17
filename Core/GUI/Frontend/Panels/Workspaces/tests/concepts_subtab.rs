//! Windowed gpui tests for the Concepts sub-tab (TBS concept-system Phase 1).
//!
//! Mount the real views in a gpui test window, drive them through the scripted
//! fake backend at the `wylde_gui_pipe::call` chokepoint, and assert on
//! observable state AND on the verbs issued — no live `wylde-workspaces` stack.
//! See `docs/gui-testing.md`. Determinism via `cx.run_until_parked()`.
//!
//! What they retire (owed feel-tests):
//!   (a) the Concepts sub-tab loads the workspace's concepts on mount via the
//!       hybrid-search verb (empty query = full set);
//!   (b) the Build button issues the cheap-concept build verb then reloads;
//!   (c) the Vocabulary tab defaults to the Vocabulary sub-tab and flips to
//!       Concepts (the child view is live and loaded).

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::vocabulary::concepts_view::ConceptsView;
use wylde_panel_workspaces::vocabulary::{VocabSubTab, VocabularyTab};

/// A `workspaces.concepts.search` reply with `n` directory concepts.
fn search_reply(n: usize) -> Value {
    let results: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "concept": {
                    "id": format!("dir:src/m{i}"),
                    "label": format!("Module {i}"),
                    "description": "a directory concept",
                    "members": [format!("sym_{i}")],
                    "member_files": [format!("src/m{i}/lib.rs")],
                    "parent_concepts": [],
                    "source": "directory_cluster"
                },
                "score": 0.0, "fuzzy": 0.0, "semantic": 0.0
            })
        })
        .collect();
    json!({ "workspace_id": "ws-a", "query": "", "count": n, "results": results })
}

/// Backend with an active workspace + a canned concept search reply.
fn backend(n: usize) -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "active_id": "ws-a", "workspaces": [{ "id": "ws-a", "folder": "C:/code/a" }] }),
        )
        .on("workspaces.concepts.search", search_reply(n))
}

// ── (a) Concepts sub-tab loads on mount ──────────────────────────────────

#[gpui::test]
fn concepts_subtab_loads_concepts_on_mount(cx: &mut TestAppContext) {
    let fake = backend(3);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ConceptsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_loading(), "load resolved");
            assert_eq!(view.workspace_id(), Some("ws-a"));
            assert_eq!(view.results_len(), 3, "three concepts shown");
        })
        .unwrap();

    // It loaded via the hybrid-search verb, scoped to the active workspace.
    let call = fake
        .last_call_for("workspaces.concepts.search")
        .expect("concepts sub-tab must load via concepts.search");
    assert_eq!(call.payload_str("workspace_id").as_deref(), Some("ws-a"));
}

// ── (b) Build issues the build verb, then reloads ────────────────────────

#[gpui::test]
fn build_button_issues_build_then_reloads(cx: &mut TestAppContext) {
    let fake = backend(0)
        .on("workspaces.concepts.build", json!({ "workspace_id": "ws-a", "built": 5 }));
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ConceptsView::new(cx));
    cx.run_until_parked();

    let searches_before = fake.count_for("workspaces.concepts.search");
    window
        .update(cx, |view, _w, cx| view.build(cx))
        .unwrap();
    cx.run_until_parked();

    let build = fake
        .last_call_for("workspaces.concepts.build")
        .expect("Build must issue the cheap-concept build verb");
    assert_eq!(build.payload_str("workspace_id").as_deref(), Some("ws-a"));
    assert!(
        fake.count_for("workspaces.concepts.search") > searches_before,
        "a build reloads the concept list"
    );
}

// ── (c) Vocabulary tab defaults to Vocabulary, flips to Concepts ─────────

#[gpui::test]
fn vocabulary_tab_switches_to_concepts_subtab(cx: &mut TestAppContext) {
    let fake = backend(2);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| VocabularyTab::new(cx));
    cx.run_until_parked();

    // Defaults to the Vocabulary sub-tab; the Concepts child still loaded
    // (it's built + spawns its load in `VocabularyTab::new`).
    window
        .update(cx, |tab, _w, cx| {
            assert_eq!(tab.sub_tab(), VocabSubTab::Vocabulary, "defaults to Vocabulary");
            assert_eq!(
                tab.concepts_view().read(cx).results_len(),
                2,
                "the Concepts child loaded its concepts"
            );
            tab.set_sub_tab(VocabSubTab::Concepts, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |tab, _w, _cx| {
            assert_eq!(tab.sub_tab(), VocabSubTab::Concepts, "switched to Concepts");
        })
        .unwrap();

    assert!(
        fake.count_for("workspaces.concepts.search") >= 1,
        "the concepts surface queried the search verb"
    );
}
