//! Windowed gpui tests for the Workspaces panel's Registry ⇄ in-workspace
//! navigation (the UX rework state machine).
//!
//! These mount a real [`WorkspacesPanel`] view in a gpui test window and drive
//! it through the enter / leave / tab-select flows, asserting on observable
//! panel state AND on the verbs it issues (via the scripted fake backend at the
//! `wylde_gui_pipe::call` chokepoint). No live `wylde-workspaces` stack runs.
//!
//! What they retire (owed feel-tests):
//!   (a) Registry is the landing/home; entering a card flips into the
//!       in-workspace view (tabs appear) AND activates the workspace.
//!   (b) the back-arrow returns to the Registry without deactivating.
//!   (c) the recency list is `list_mru`-ordered — row 0 is the most recent,
//!       which also seeds the default active workspace (no separate hero).
//!   (d) a cross-panel focus selects an in-workspace tab; the `"registry"`
//!       key routes to the back-arrow (leave), not a tab.
//!
//! Determinism: every async effect is driven to quiescence with
//! `cx.run_until_parked()` before asserting. See `docs/gui-testing.md`.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_pipe::WorkspaceFocus;
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::tabs::WorkspacesTab;
use wylde_panel_workspaces::WorkspacesPanel;

/// One `workspaces.list_mru` row. The store emits `folder` (the panel falls
/// back to `path`); `file_count`/`last_indexed_at` carry the index state.
fn ws_row(id: &str, folder: &str, file_count: u64, indexed_epoch: f64) -> Value {
    json!({
        "id": id,
        "folder": folder,
        "file_count": file_count,
        "last_indexed_at": indexed_epoch,
        "indexing": false,
    })
}

/// Script `workspaces.list_mru` to return `rows` (MRU order, newest first).
fn with_list(rows: Vec<Value>) -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new().on("workspaces.list_mru", json!({ "workspaces": rows }))
}

/// Mount a `WorkspacesPanel` and kick its first list load (the factory does
/// this in `view()`; in tests we mount the bare view + call `spawn_refresh`).
fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<WorkspacesPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = WorkspacesPanel::new();
        WorkspacesPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

// ── (a) enter a card → in-workspace view + activate ──────────────────────

#[gpui::test]
fn entering_a_card_enters_the_workspace_and_activates_it(cx: &mut TestAppContext) {
    let fake = with_list(vec![
        ws_row("ws-a", "C:/code/a", 10, 1.0),
        ws_row("ws-b", "C:/code/b", 20, 1.0),
    ]);
    let _guard = fake.clone().install();

    let window = mount(cx);

    // Landed on the Registry: not inside any workspace, MRU head is the
    // default-active row (no separate hero card).
    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.entered.is_none(), "fresh panel lands on the Registry");
            assert_eq!(panel.active_id.as_deref(), Some("ws-a"));
        })
        .unwrap();

    // Click a card (enter the SECOND workspace, not the active head).
    window
        .update(cx, |panel, _w, cx| {
            panel.enter_workspace("ws-b".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.entered.as_deref(),
                Some("ws-b"),
                "entering a card flips the panel into the in-workspace view"
            );
            assert_eq!(
                panel.tab,
                WorkspacesTab::Files,
                "entering lands on the Files tab (in-workspace landing)"
            );
            assert_eq!(
                panel.active_id.as_deref(),
                Some("ws-b"),
                "entering a card also makes that workspace active"
            );
        })
        .unwrap();

    // And it persisted that activation via the same verb "Switch" used.
    let set = fake
        .last_call_for("workspaces.set_active")
        .expect("entering a card must persist the active workspace");
    assert_eq!(
        set.payload_str("workspace_id").as_deref(),
        Some("ws-b"),
        "set_active must carry the entered workspace_id"
    );
}

// ── (b) back-arrow → Registry, without deactivating ──────────────────────

#[gpui::test]
fn back_arrow_returns_to_registry_keeping_active(cx: &mut TestAppContext) {
    let fake = with_list(vec![ws_row("ws-a", "C:/code/a", 10, 1.0)]);
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, cx| {
            panel.enter_workspace("ws-a".to_owned(), cx)
        })
        .unwrap();
    cx.run_until_parked();
    let set_active_before = fake.count_for("workspaces.set_active");

    // Back arrow.
    window
        .update(cx, |panel, _w, cx| panel.leave_workspace(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.entered.is_none(),
                "the back arrow returns to the Registry landing"
            );
            assert_eq!(
                panel.active_id.as_deref(),
                Some("ws-a"),
                "leaving does NOT deactivate the workspace"
            );
        })
        .unwrap();
    assert_eq!(
        fake.count_for("workspaces.set_active"),
        set_active_before,
        "leaving issues no set_active (it doesn't change activation)"
    );
}

// ── (c) recency-ordered list; row 0 is the most recent (no hero) ─────────

#[gpui::test]
fn registry_list_is_mru_ordered_and_head_seeds_active(cx: &mut TestAppContext) {
    // MRU order, newest first: c, a, b.
    let fake = with_list(vec![
        ws_row("c", "C:/c", 1, 1.0),
        ws_row("a", "C:/a", 2, 1.0),
        ws_row("b", "C:/b", 3, 1.0),
    ]);
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, _cx| {
            let ids: Vec<&str> = panel.workspaces.iter().map(|w| w.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["c", "a", "b"],
                "the list preserves MRU order — row 0 is the most recent"
            );
            // The MRU head seeds the default-active workspace: the single
            // uniform list carries the 'most recent' itself, so no separate
            // 'ACTIVE WORKSPACE' hero card is needed (UX rework decision 3).
            assert_eq!(
                panel.active_id.as_deref(),
                Some("c"),
                "the MRU head (row 0) is the default-active workspace"
            );
        })
        .unwrap();
}

// ── (d) cross-panel focus selects a tab; 'registry' leaves ───────────────

#[gpui::test]
fn focus_selects_in_workspace_tab(cx: &mut TestAppContext) {
    let fake = with_list(vec![ws_row("ws-a", "C:/code/a", 10, 1.0)]);
    let _guard = fake.clone().install();

    let window = mount(cx);

    // A focus that names a tab ENTERS the active workspace (tabs only exist
    // in-workspace) and selects that tab — the focus-bus drain path (S7).
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_nav_focus(
                WorkspaceFocus {
                    tab: Some("graph".to_owned()),
                    node_id: None,
                },
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.entered.as_deref(),
                Some("ws-a"),
                "a tab focus from the Registry must first enter the active workspace"
            );
            assert_eq!(
                panel.tab,
                WorkspacesTab::Graph,
                "focus selected the Graph tab"
            );
        })
        .unwrap();

    // Now switch to the Editor tab the same way (tab-shell switching).
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_nav_focus(
                WorkspaceFocus {
                    tab: Some("editor".to_owned()),
                    node_id: None,
                },
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.tab,
                WorkspacesTab::Editor,
                "tab-shell switched to Editor"
            );
            assert_eq!(
                panel.entered.as_deref(),
                Some("ws-a"),
                "still in the workspace"
            );
        })
        .unwrap();

    // The 'registry' key is the home, not a tab — it routes to the back arrow.
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_nav_focus(
                WorkspaceFocus {
                    tab: Some("registry".to_owned()),
                    node_id: None,
                },
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.entered.is_none(),
                "the 'registry' focus key leaves the workspace (back to home)"
            );
        })
        .unwrap();
}

// ── (e) GUI-wide service control: Start / Restart the workspaces service ──

#[gpui::test]
fn start_service_affordance_issues_lifecycle_start(cx: &mut TestAppContext) {
    // A down service surfaces the "Start service" button; clicking it drives
    // `service.start` against the lifecycle daemon for the workspaces service,
    // then re-reads the list (which clears the banner on success).
    let fake = with_list(vec![ws_row("ws-a", "C:/code/a", 10, 1.0)]);
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, _cx| {
            panel.error = Some("pipe_unavailable: service not running".to_owned());
        })
        .unwrap();

    // Click "Start service".
    window
        .update(cx, |_panel, _w, cx| {
            WorkspacesPanel::spawn_service_control(false, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let start = fake
        .last_call_for("service.start")
        .expect("Start service must dispatch the lifecycle service.start verb");
    assert_eq!(
        start.service, "wylde-lifecycle",
        "service control hits the lifecycle daemon"
    );
    assert_eq!(
        start.payload_str("name").as_deref(),
        Some("wylde-workspaces"),
        "the workspaces panel starts the wylde-workspaces service"
    );
    // A successful control re-read clears the banner.
    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_none(),
                "a successful start clears the error banner"
            );
        })
        .unwrap();
}

#[gpui::test]
fn restart_service_affordance_issues_lifecycle_restart(cx: &mut TestAppContext) {
    // An out-of-date service surfaces "Restart service" → `service.restart`.
    let fake = with_list(vec![ws_row("ws-a", "C:/code/a", 10, 1.0)]);
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |_panel, _w, cx| {
            WorkspacesPanel::spawn_service_control(true, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let restart = fake
        .last_call_for("service.restart")
        .expect("Restart service must dispatch the lifecycle service.restart verb");
    assert_eq!(restart.service, "wylde-lifecycle");
    assert_eq!(
        restart.payload_str("name").as_deref(),
        Some("wylde-workspaces")
    );
    assert_eq!(
        fake.count_for("service.start"),
        0,
        "restart must NOT use the start verb (it picks up a rebuilt binary)"
    );
}
