//! L7 panel-walk — Tools (issue #35, roadmap T0.1b).
//!
//! One of the four zero-coverage panels. Mount the real `ToolsPanel` the way
//! the Shell does (`new()` + `spawn_refresh`), drive the extension-bridge IPC
//! through the scripted fake, and assert it survives every realistic backend
//! condition.
//!
//! **What "error state" means for Tools:** a panel-level `error: Option<String>`
//! set when `ext.list` fails, plus a `loading: bool` that must clear once the
//! first reply lands (a panel stuck `loading == true` is the "stuck spinner"
//! failure). The gate: `error.is_none()` + `!loading` on the happy path;
//! `error.is_some()` (surfaced, not swallowed) when the bridge is down.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_tools::ToolsPanel;

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ToolsPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = ToolsPanel::new();
        ToolsPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn tools_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ext.list",
            json!({ "extensions": [
                { "id": "ext-a", "name": "Ext A", "version": "1.0.0", "enabled": true },
            ]}),
        )
        .on("extensions.list_panels", json!({ "panels": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(!panel.loading, "the loading spinner clears once the list arrives");
            assert_eq!(panel.extensions.len(), 1, "the extension row loaded");
        })
        .unwrap();
    assert_eq!(fake.count_for("ext.list"), 1);
}

#[gpui::test]
fn tools_survives_backend_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("ext.list", "pipe_unavailable: extension-bridge not running")
        .on_err("extensions.list_panels", "pipe_unavailable: extension-bridge not running");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "a down bridge surfaces a visible error — not a silent empty list"
            );
            assert!(!panel.loading, "the spinner still clears on failure (no stuck spinner)");
        })
        .unwrap();
}

#[gpui::test]
fn tools_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err("ext.list", "internal_error: bridge blew up");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_some(), "an error envelope surfaces on the panel");
            assert!(!panel.loading);
        })
        .unwrap();
}

#[gpui::test]
fn tools_tolerates_empty_backend(cx: &mut TestAppContext) {
    // Default fake → every action returns Ok({}); `ext.list` parses to an empty
    // extension list. Empty is NOT an error here.
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "empty ok-replies are not an error");
            assert!(!panel.loading);
            assert!(panel.extensions.is_empty(), "an empty bridge yields an empty list");
        })
        .unwrap();
}
