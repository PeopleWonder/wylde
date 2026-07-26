//! L7 **control**-walk — Workspaces (issue #247).
//!
//! Workspaces is a container of independently-mountable sub-views. This file
//! walks the **Hierarchy** sub-view, which is self-contained: all of its
//! controls act on its own state, so it walks cleanly when mounted alone.
//!
//! The other surfaces are staged deliberately:
//!
//! * **Concepts / Relations / Vocabulary** render a sub-tab switcher whose
//!   click switches the *parent* `VocabularyTab`'s active tab. Mounted
//!   standalone they have no parent to switch, so those pills produce no
//!   observable effect in isolation — they are not dead, they need the
//!   container. They are walked through `VocabularyTab` in a follow-up
//!   (#247, Workspaces walk part 2), where the tab state is real.
//! * The **graph canvas** + the main `WorkspacesPanel` chrome carry heavier
//!   IPC + physics state and are walked there too.
//!
//! All 13 Workspaces source files are already routed through `control()` (this
//! batch), so `wylde_check` rule 59 enforces every one of them; the staged
//! work above is about *walking* them, not routing them.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::control_walk::ControlWalk;
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::hierarchy::HierarchyView;
use wylde_panel_workspaces::ipc::WorkspaceSummary;
use wylde_panel_workspaces::routing::DependencyTreeView;
use wylde_panel_workspaces::tabs::WorkspacesTab;
use wylde_panel_workspaces::workspaces_panel::{ModelPull, PullPhase};
use wylde_panel_workspaces::WorkspacesPanel;

fn tree_reply() -> Value {
    json!({
        "workspace_id": "ws-a",
        "enabled": true,
        "roots": [{
            "concept_id": "root", "label": "Root", "children": [
                { "concept_id": "child", "label": "Child", "children": [] }
            ]
        }],
        "unplaced": [{ "concept_id": "loose", "label": "Loose" }]
    })
}

#[gpui::test]
fn every_hierarchy_control_does_something(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "active_id": "ws-a", "workspaces": [{ "id": "ws-a", "folder": "C:/code/a" }] }),
        )
        .on("workspaces.hierarchy.get_tree", tree_reply());
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| HierarchyView::new(cx));
    cx.run_until_parked();

    ControlWalk::new(window, &fake)
        .fingerprint(|v: &HierarchyView| {
            format!(
                "loading={} enabled={} ws={:?}",
                v.is_loading(),
                v.is_enabled(),
                v.workspace_id(),
            )
        })
        .sources(&[include_str!("../src/hierarchy/mod.rs")])
        .run(cx)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

/// The dependency-tree canvas (`routing-tree-canvas`, the R3b typed-edge tree).
///
/// Its one control is the canvas: a click hit-tests a node, re-centres the
/// camera on it (a no-op when the clicked node is already centred), and emits
/// `TreeEvent::Selected` — a handoff to the host that deep-links the editor.
/// Mounted standalone there is no host to observe the emit, so the canvas is
/// declared `external_effect` (clicked for panic-safety over a loaded tree; the
/// emit + re-centre have no delta this isolated view can assert). It runs in CI
/// via the `panel-walk` alias.
#[gpui::test]
fn every_dependency_tree_control_does_something(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on("workspaces.list_mru", json!({ "active_id": "ws-a" }))
        .on("workspaces.concepts.search", json!({ "results": [] }))
        .on("workspaces.anchors.list", json!({ "anchors": [] }))
        .on(
            "workspaces.concepts.relations.graph",
            json!({ "relations": [
                { "from": {"node":"concept","id":"a"}, "to": {"node":"concept","id":"b"}, "kind": "dependency" }
            ]}),
        );
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| DependencyTreeView::new(cx));
    cx.run_until_parked();

    ControlWalk::new(window, &fake)
        .fingerprint(|v: &DependencyTreeView| {
            format!(
                "loading={} nodes={} edges={} ws={:?}",
                v.is_loading(),
                v.node_count(),
                v.edge_count(),
                v.workspace_id(),
            )
        })
        .external_effect(&["routing-tree-canvas"])
        .sources(&[include_str!("../src/routing/tree_view.rs")])
        .run(cx)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

/// One MRU registry row. `WorkspaceSummary` derives `Default` and is all-`pub`.
fn ws_summary(id: &str, path: &str) -> WorkspaceSummary {
    WorkspaceSummary {
        id: id.to_owned(),
        path: path.to_owned(),
        ..Default::default()
    }
}

/// Fold the in-flight model-pull phase into a stable discriminant so the
/// fingerprint observes `workspaces-download-model` flipping Offered ->
/// Downloading/Failed. That strip's only effect is on `pull` (its `ollama.pull`
/// stream helper goes through `stream_call`, which the ScriptedBackend does NOT
/// record). `PullPhase` has no `Debug`, hence the hand-written match.
fn pull_tag(pull: &Option<ModelPull>) -> &'static str {
    match pull {
        None => "none",
        Some(p) => match p.phase {
            PullPhase::Offered => "offered",
            PullPhase::Downloading(_) => "downloading",
            PullPhase::Failed(_) => "failed",
        },
    }
}

/// The Workspaces **registry / container** chrome: the Add button (a folder
/// picker, now suppressed + observed via the `native_dialog` channel), the
/// workspace cards + reindex/remove, the in-workspace tab bar + back, the
/// service-recovery strips, and the model-download strip. Its child tab views
/// (Files/Editor/Vocabulary/Graph/Settings) are walked in their own files, so
/// this leaves them `None` and stays on the panel's own chrome.
#[gpui::test]
fn every_workspaces_panel_control_does_something(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({
            "active_id": "ws-a",
            "workspaces": [
                { "id": "ws-a", "folder": "C:/code/a" },
                { "id": "ws-b", "folder": "C:/code/b" },
            ]
        }),
    );
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| {
        let panel = WorkspacesPanel::new();
        WorkspacesPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();

    ControlWalk::new(window, &fake)
        .fingerprint(|v: &WorkspacesPanel| {
            let idx: String = v
                .workspaces
                .iter()
                .map(|w| if w.indexing { '1' } else { '0' })
                .collect();
            format!(
                "entered={:?} tab={:?} err={} loading={} n={} active={:?} pull={} idx={}",
                v.entered,
                v.tab,
                v.error.is_some(),
                v.loading,
                v.workspaces.len(),
                v.active_id,
                pull_tag(&v.pull),
                idx,
            )
        })
        .reset(|v: &mut WorkspacesPanel, _w, cx| {
            v.loading = false;
            v.error = None;
            v.entered = None;
            v.pull = None;
            v.active_id = Some("ws-a".to_owned());
            v.tab = WorkspacesTab::Registry;
            v.workspaces = vec![
                ws_summary("ws-a", "C:/code/a"),
                ws_summary("ws-b", "C:/code/b"),
            ];
            cx.notify();
        })
        // In-workspace view: paints `ws-back` + the `ws-tab::{label}` bar.
        .state("in-workspace", |v: &mut WorkspacesPanel, _w, cx| {
            v.entered = Some("ws-a".to_owned());
            cx.notify();
        })
        // Down-service error strip: start-service + retry.
        .state("svc-down", |v: &mut WorkspacesPanel, _w, cx| {
            v.error = Some("pipe_unavailable: wylde-workspaces is not running".to_owned());
            cx.notify();
        })
        // Out-of-date error strip: restart-service (+ retry).
        .state("svc-outofdate", |v: &mut WorkspacesPanel, _w, cx| {
            v.error = Some("no_action: unknown action workspaces.reindex".to_owned());
            cx.notify();
        })
        // Model-download strip: download-model (phase Offered).
        .state("pull-offered", |v: &mut WorkspacesPanel, _w, cx| {
            v.pull = Some(ModelPull {
                model: "nomic-embed-text".to_owned(),
                retry_id: "ws-a".to_owned(),
                phase: PullPhase::Offered,
            });
            cx.notify();
        })
        .sources(&[include_str!("../src/workspaces_panel.rs")])
        .run(cx)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}
