//! L7 panel-walk — Devices (issue #35, roadmap T0.1b).
//!
//! Devices already has behavioural coverage (`cancel_pairing.rs`); this file is
//! the uniform mount-under-every-backend-condition smoke that the panel-walk
//! gate runs across all 9 panels. Mount the real `DevicesPanel` the way the
//! Shell does (`new()` + `spawn_refresh`).
//!
//! **What "error state" means for Devices:** `error: Option<String>` set when
//! `device_gate.list_devices` fails, and `loading_devices: bool` that must
//! clear. Gate: `error.is_none()` + `!loading_devices` on the happy path;
//! `error.is_some()` when device-gate is down.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_devices::DevicesPanel;

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<DevicesPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = DevicesPanel::new();
        DevicesPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn devices_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "device_gate.list_devices",
        json!({ "devices": [
            { "id": "d1", "label": "Pixel", "tier": "read_only" },
        ]}),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(!panel.loading_devices, "the devices spinner cleared");
            assert!(panel.pairing.is_none(), "no pairing window open at rest");
        })
        .unwrap();
    assert_eq!(fake.count_for("device_gate.list_devices"), 1);
}

#[gpui::test]
fn devices_survives_backend_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("device_gate.list_devices", "pipe_unavailable: device-gate not running");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "a down device-gate surfaces a visible error"
            );
            assert!(!panel.loading_devices, "no stuck spinner on failure");
        })
        .unwrap();
}

#[gpui::test]
fn devices_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("device_gate.list_devices", "internal_error: device-gate blew up");
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_some(), "an error envelope surfaces on the panel");
            assert!(!panel.loading_devices);
        })
        .unwrap();
}

#[gpui::test]
fn devices_tolerates_empty_backend(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no paired devices is not an error");
            assert!(!panel.loading_devices);
        })
        .unwrap();
}
