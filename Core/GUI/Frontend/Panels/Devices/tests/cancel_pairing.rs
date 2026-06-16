//! Windowed gpui tests for the Devices "Cancel pairing" control
//! (GUI-responsiveness pass — category c: swallowed result). The fix: a cancel
//! failure must surface, because the card is closed optimistically but a failed
//! backend cancel leaves the server-side pairing window open (a device could
//! still complete against the code).
//!
//! Mount a real DevicesPanel and drive start/cancel through the scripted fake
//! backend at the `wylde_gui_pipe::call` seam — no live device-gate service.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_devices::DevicesPanel;

/// Open a pairing window so there's a card to cancel.
fn open_pairing(
    cx: &mut TestAppContext,
    fake: std::sync::Arc<ScriptedBackend>,
) -> (gpui::WindowHandle<DevicesPanel>, wylde_gui_test_support::BackendGuard) {
    let fake = fake.on(
        "device_gate.start_pairing",
        json!({ "code": "123456", "expires_at": 9_999_999_999.0_f64 }),
    );
    let guard = fake.install();
    let window = cx.add_window(|_w, _cx| DevicesPanel::new());
    cx.run_until_parked();
    window.update(cx, |p, _w, cx| p.start_pairing(cx)).unwrap();
    cx.run_until_parked();
    window
        .update(cx, |p, _w, _cx| assert!(p.pairing.is_some(), "pairing window opened"))
        .unwrap();
    (window, guard)
}

#[gpui::test]
fn cancel_pairing_dispatches_the_cancel_verb_and_clears_the_card(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("device_gate.cancel_pairing", json!({ "cancelled": true }));
    let (window, _guard) = open_pairing(cx, fake.clone());

    window.update(cx, |p, _w, cx| p.cancel_pairing(cx)).unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("device_gate.cancel_pairing")
        .expect("Cancel must dispatch device_gate.cancel_pairing");
    assert_eq!(call.service, "wylde-device-gate");
    window
        .update(cx, |p, _w, _cx| {
            assert!(p.pairing.is_none(), "the pairing card is cleared");
            assert!(p.error.is_none(), "a clean cancel surfaces no error");
        })
        .unwrap();
}

#[gpui::test]
fn a_failed_cancel_against_a_down_service_is_surfaced(cx: &mut TestAppContext) {
    // The pre-fix symptom: `let _ = cancel_pairing(...)` swallowed the result,
    // so a failed cancel left the server pairing window open with ZERO user
    // signal. Now the failure is captured and surfaced. We model the real
    // failure mode — the device-gate service is DOWN, so both the cancel AND
    // the follow-up refresh fail — and assert the panel ends in a visible error
    // state (not silently "all good"), with the cancel verb actually attempted.
    let fake = ScriptedBackend::new()
        .on_err("device_gate.cancel_pairing", "pipe_unavailable: device-gate not running")
        .on_err("device_gate.list_devices", "pipe_unavailable: device-gate not running");
    let (window, _guard) = open_pairing(cx, fake.clone());

    window.update(cx, |p, _w, cx| p.cancel_pairing(cx)).unwrap();
    cx.run_until_parked();

    assert_eq!(
        fake.count_for("device_gate.cancel_pairing"),
        1,
        "the cancel was actually attempted against the backend"
    );
    window
        .update(cx, |p, _w, _cx| {
            assert!(
                p.error.is_some(),
                "a failed cancel against a down service leaves a visible error — not swallowed silence"
            );
        })
        .unwrap();
}
