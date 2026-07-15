//! L7 panel-walk — Memory (issue #35, roadmap T0.1b).
//!
//! Memory already has behavioural coverage (`copy_in.rs`); this is the uniform
//! mount-under-every-backend-condition smoke. Mount the real `MemoryPanel` the
//! way the Shell does (`new(cx)` + `spawn_refresh`).
//!
//! **What "error state" means for Memory:** `error: Option<String>` set when the
//! long-term list (or search) read fails, plus `loading_long_term` /
//! `loading_workspaces` / `loading_short_term` flags that must clear. Gate:
//! `error.is_none()` + spinners cleared on the happy path; `error.is_some()`
//! when the harness is down.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_memory::MemoryPanel;

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<MemoryPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = MemoryPanel::new(cx);
        MemoryPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn memory_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "memory.long_term.list",
            json!({ "memories": [
                { "id": "m1", "body": "prefers terse answers", "importance": 9,
                  "source": "reflection", "created_at": 0.0, "last_used_at": 0.0, "tags": [] },
            ]}),
        )
        .on("workspaces.list_mru", json!({ "workspaces": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(!panel.loading_long_term, "the long-term spinner cleared");
            assert_eq!(panel.long_term.len(), 1, "the curated row loaded");
        })
        .unwrap();
    assert_eq!(fake.count_for("memory.long_term.list"), 1);
}

#[gpui::test]
fn memory_survives_backend_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("memory.long_term.list", "pipe_unavailable: harness not running")
        .on_err("workspaces.list_mru", "pipe_unavailable: workspaces not running");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "a down harness surfaces a visible error on the long-term section"
            );
            assert!(!panel.loading_long_term, "no stuck spinner on failure");
        })
        .unwrap();
}

#[gpui::test]
fn memory_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("memory.long_term.list", "internal_error: memory store blew up");
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_some(), "an error envelope surfaces on the panel");
            assert!(!panel.loading_long_term);
        })
        .unwrap();
}

#[gpui::test]
fn memory_tolerates_empty_backend(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no curated memories is not an error");
            assert!(!panel.loading_long_term);
            assert!(panel.long_term.is_empty(), "an empty store yields an empty list");
        })
        .unwrap();
}
