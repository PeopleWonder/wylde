//! L7 panel-walk — Workspaces + its sub-surfaces (issue #35, roadmap T0.1b).
//!
//! The Workspaces panel is the widest surface: the top-level `WorkspacesTab`
//! bar (Registry / Files / Editor / Graph / Vocabulary / Conversations /
//! Settings) plus the Vocabulary tab's own `VocabSubTab` bar (Vocabulary /
//! Concepts / Relations / Hierarchy) with three independently-mounted child
//! views. The brief counts these as the "~7 Workspaces subtabs" — this file
//! walks them all.
//!
//! **What "error state" means for Workspaces:** a panel-level
//! `error: Option<String>` on `WorkspacesPanel` (set from pipe failures on the
//! registry load), plus `loading: bool`. Each child view carries its own state
//! and exposes an `is_loading()` accessor. There is no error *enum* — the gate
//! is `error.is_none()` + a coherent (possibly empty) registry on the happy
//! path, `error.is_some()` when the store is down, and every subtab mounting +
//! switching without panic.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::hierarchy::HierarchyView;
use wylde_panel_workspaces::routing::RelationsView;
use wylde_panel_workspaces::tabs::WorkspacesTab;
use wylde_panel_workspaces::vocabulary::concepts_view::ConceptsView;
use wylde_panel_workspaces::vocabulary::{VocabSubTab, VocabularyTab};
use wylde_panel_workspaces::WorkspacesPanel;

fn ws_row(id: &str, folder: &str) -> Value {
    json!({ "id": id, "folder": folder })
}

/// Mount the WorkspacesPanel the way `WorkspacesPanel::view` does — bare `new`
/// + `spawn_refresh` (mirrors the existing `registry_nav.rs` helper).
fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<WorkspacesPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = WorkspacesPanel::new();
        WorkspacesPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

// ── The WorkspacesPanel registry, under every backend condition ──────────

#[gpui::test]
fn workspaces_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "active_id": "ws-a", "workspaces": [ws_row("ws-a", "C:/a"), ws_row("ws-b", "C:/b")] }),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(!panel.loading, "the registry spinner cleared");
            assert_eq!(
                panel.tab,
                WorkspacesTab::Registry,
                "lands on the Registry tab"
            );
        })
        .unwrap();
    assert_eq!(fake.count_for("workspaces.list_mru"), 1);
}

#[gpui::test]
fn workspaces_survives_backend_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err(
        "workspaces.list_mru",
        "pipe_unavailable: workspaces not running",
    );
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "a down workspaces store surfaces a visible error"
            );
            assert!(!panel.loading, "no stuck spinner on failure");
        })
        .unwrap();
}

#[gpui::test]
fn workspaces_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err(
        "workspaces.list_mru",
        "internal_error: workspaces store blew up",
    );
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "an error envelope surfaces on the panel"
            );
            assert!(!panel.loading);
        })
        .unwrap();
}

#[gpui::test]
fn workspaces_tolerates_empty_backend(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "an empty registry is not an error");
            assert!(!panel.loading);
        })
        .unwrap();
}

// ── Every top-level tab selects without panicking ────────────────────────

#[gpui::test]
fn workspaces_every_tab_selects_without_panic(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "active_id": "ws-a", "workspaces": [ws_row("ws-a", "C:/a")] }),
    );
    let _guard = fake.install();

    let window = mount(cx);

    // Enter a workspace so the in-workspace tabs have context, then visit each
    // top-level surface. Setting `panel.tab` is exactly what the tab-bar button
    // handler does; driving to quiescence after each proves the switch + any
    // spawned load settles without panic.
    window
        .update(cx, |panel, _w, cx| {
            panel.enter_workspace("ws-a".to_owned(), cx)
        })
        .unwrap();
    cx.run_until_parked();

    for tab in [
        WorkspacesTab::Registry,
        WorkspacesTab::Files,
        WorkspacesTab::Editor,
        WorkspacesTab::Graph,
        WorkspacesTab::Vocabulary,
        WorkspacesTab::Conversations,
        WorkspacesTab::Settings,
    ] {
        window
            .update(cx, |panel, _w, _cx| {
                panel.tab = tab;
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |panel, _w, _cx| {
                assert_eq!(panel.tab, tab, "switched to {tab:?}");
                assert!(panel.error.is_none(), "no error selecting {tab:?}");
            })
            .unwrap();
    }
}

// ── The Vocabulary sub-tab bar: every VocabSubTab switches ───────────────

#[gpui::test]
fn vocabulary_every_subtab_switches_without_panic(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "active_id": "ws-a", "workspaces": [ws_row("ws-a", "C:/a")] }),
    );
    let _guard = fake.install();

    let window = cx.add_window(|_w, cx| VocabularyTab::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |tab, _w, _cx| {
            assert_eq!(
                tab.sub_tab(),
                VocabSubTab::Vocabulary,
                "defaults to Vocabulary"
            );
        })
        .unwrap();

    for sub in [
        VocabSubTab::Concepts,
        VocabSubTab::Relations,
        VocabSubTab::Hierarchy,
        VocabSubTab::Vocabulary,
    ] {
        window
            .update(cx, |tab, _w, cx| tab.set_sub_tab(sub, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |tab, _w, _cx| {
                assert_eq!(tab.sub_tab(), sub, "switched to {sub:?}");
            })
            .unwrap();
    }
}

// ── Each Vocabulary child view mounts + loads without panic ──────────────
//
// Mounted in isolation (the same pattern as concepts_subtab.rs /
// hierarchy_subtab.rs / relations_subtab.rs) against the default empty backend
// — the Tier-B "does this subtab load without panicking / stuck spinner" gate.

#[gpui::test]
fn concepts_view_mounts_and_settles(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "active_id": "ws-a", "workspaces": [ws_row("ws-a", "C:/a")] }),
    );
    let _guard = fake.install();

    let window = cx.add_window(|_w, cx| ConceptsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(
                !view.is_loading(),
                "the Concepts view settles (no stuck spinner)"
            );
        })
        .unwrap();
}

#[gpui::test]
fn hierarchy_view_mounts_and_settles(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "active_id": "ws-a", "workspaces": [ws_row("ws-a", "C:/a")] }),
    );
    let _guard = fake.install();

    let window = cx.add_window(|_w, cx| HierarchyView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_loading(), "the Hierarchy view settles");
        })
        .unwrap();
}

#[gpui::test]
fn relations_view_mounts_and_settles(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "active_id": "ws-a", "workspaces": [ws_row("ws-a", "C:/a")] }),
    );
    let _guard = fake.install();

    let window = cx.add_window(|_w, cx| RelationsView::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(!view.is_loading(), "the Relations view settles");
        })
        .unwrap();
}
