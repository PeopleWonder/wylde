//! Windowed gpui tests for the Devices **happy-path pairing** flow — the
//! critical-path control #35 names as "pair device". Its sibling
//! `cancel_pairing.rs` covers the abort path; this covers the path a user
//! actually takes: open a pairing window, a phone completes against the code,
//! the card closes on its own and the new device appears in the list.
//!
//! No real peer device is involved. The panel never talks to the phone — it
//! polls `device_gate.get_pairing_status`, and "the phone completed" is just
//! the server reporting `{pairing_active: false}`. That makes the whole flow
//! scriptable at the `wylde_gui_pipe::call` seam.
//!
//! The poll loop waits on a gpui `background_executor().timer()`, so the tests
//! drive it with `advance_clock` rather than sleeping — deterministic, and it
//! runs in microseconds.

use std::sync::Arc;

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::{BackendGuard, ScriptedBackend};
use wylde_panel_devices::devices_panel::PAIR_POLL_INTERVAL;
use wylde_panel_devices::DevicesPanel;

/// Far enough out that the countdown can never expire the card mid-test.
/// `spawn_tick_loop` closes a card whose `expires_at` has passed against the
/// **wall clock**, and a card closing for that reason would make these tests
/// pass for the wrong reason.
const NEVER_EXPIRES: f64 = 9_999_999_999.0;

/// Mount a panel wired like the real `view()` — initial refresh + poll loop.
///
/// Deliberately **without** `spawn_tick_loop`: it's the only thing that writes
/// `now_secs`, and leaving it at 0.0 keeps the poll loop's device-list
/// subdivide (`now_secs % DEVICES_POLL_INTERVAL`) deterministic instead of
/// keyed to the wall clock. The card-expiry behaviour it drives is not what
/// these tests are about.
fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<DevicesPanel> {
    cx.add_window(|_w, cx| {
        let panel = DevicesPanel::new();
        DevicesPanel::spawn_refresh(cx);
        DevicesPanel::spawn_poll_loop(cx);
        panel
    })
}

/// Install `fake`, mount, and open a pairing card. Returns the window AND the
/// install guard — keep the guard alive for the test body (dropping it clears
/// the thread-local fake).
fn open_card(
    cx: &mut TestAppContext,
    fake: Arc<ScriptedBackend>,
) -> (gpui::WindowHandle<DevicesPanel>, BackendGuard) {
    let guard = fake.install();
    let window = mount(cx);
    cx.run_until_parked();
    window.update(cx, |p, _w, cx| p.start_pairing(cx)).unwrap();
    cx.run_until_parked();
    (window, guard)
}

#[gpui::test]
fn a_phone_completing_pairing_closes_the_card_and_lands_the_new_device(cx: &mut TestAppContext) {
    // The server opens a window, then reports it closed — which is exactly what
    // it reports once a phone has paired against the code.
    let fake = ScriptedBackend::new()
        .on(
            "device_gate.start_pairing",
            json!({ "code": "123456", "expires_at": NEVER_EXPIRES }),
        )
        .on(
            "device_gate.get_pairing_status",
            json!({ "pairing_active": false }),
        )
        .on(
            "device_gate.list_devices",
            json!({ "devices": [{
                "device_id": "dev-newphone",
                "name": "Aaron's Pixel",
                "tier": "read_only",
                "paired_at": 1_784_226_899.0,
                "last_seen": 1_784_226_899.0,
                "is_active": true,
            }] }),
        );
    let (window, _guard) = open_card(cx, fake.clone());

    window
        .update(cx, |p, _w, _cx| {
            let card = p.pairing.as_ref().expect("the pairing card opens");
            assert_eq!(card.code, "123456", "the card shows the server's code");
        })
        .unwrap();

    // Let one poll tick fire: the server now says the window is closed.
    cx.executor().advance_clock(PAIR_POLL_INTERVAL);
    cx.run_until_parked();

    assert!(
        fake.count_for("device_gate.get_pairing_status") >= 1,
        "the open card actually polls the server for completion"
    );
    window
        .update(cx, |p, _w, _cx| {
            assert!(
                p.pairing.is_none(),
                "the card closes itself once the server reports the window inactive — \
                 the user should not have to dismiss it"
            );
            assert!(
                p.devices.iter().any(|d| d.device_id == "dev-newphone"),
                "the freshly-paired device is refreshed into the list, not left \
                 invisible until the next 10s poll"
            );
            assert!(p.error.is_none(), "a successful pairing surfaces no error");
        })
        .unwrap();
}

#[gpui::test]
fn a_transient_status_failure_keeps_the_card_open(cx: &mut TestAppContext) {
    // The poll loop's `Err(_) => { /* keep the card; transient */ }` branch.
    // This matters: a blipping device-gate must NOT tear down a pairing window
    // the user is mid-way through typing a code into. Closing on a transient
    // read would strand them with a code the server still considers live.
    let fake = ScriptedBackend::new()
        .on(
            "device_gate.start_pairing",
            json!({ "code": "654321", "expires_at": NEVER_EXPIRES }),
        )
        .on_err(
            "device_gate.get_pairing_status",
            "pipe_unavailable: device-gate not running",
        );
    let (window, _guard) = open_card(cx, fake.clone());

    cx.executor().advance_clock(PAIR_POLL_INTERVAL);
    cx.run_until_parked();

    assert!(
        fake.count_for("device_gate.get_pairing_status") >= 1,
        "the status read was actually attempted"
    );
    window
        .update(cx, |p, _w, _cx| {
            let card = p
                .pairing
                .as_ref()
                .expect("a transient status failure must not close the pairing window");
            assert_eq!(card.code, "654321", "the code survives the failed poll");
        })
        .unwrap();
}
