//! L7 **control**-walk — Devices (issue #247).
//!
//! Devices has several occluding cards/dialogs — the pairing card, the
//! per-device revoke-confirm, the tier-escalation-confirm, and the
//! rotated-token card. It uses the walk's `.reset()` (close them all before
//! each click) plus one `.state()` per card so its controls paint and get
//! walked. The pairing card is opened by driving the real `start_pairing`
//! flow (as `cancel_pairing.rs` does); the confirm/rotated cards are plain
//! public `Option` fields, set directly.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_devices::devices_panel::RotatedToken;
use wylde_panel_devices::DevicesPanel;

fn fingerprint(p: &DevicesPanel) -> String {
    format!(
        "devices={} err={:?} loading={} pairing={} revoke={:?} tier={:?} rotated={:?}",
        p.devices.len(),
        p.error,
        p.loading_devices,
        p.pairing.is_some(),
        p.confirm_revoke,
        p.confirm_tier_escalation,
        p.rotated.as_ref().map(|r| &r.device_id),
    )
}

fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "device_gate.list_devices",
            json!({ "devices": [
                // The tier field is `device_id` (not `id`), and it is set to a
                // value OUTSIDE the known tier set on purpose. The tier row is a
                // segmented control: whichever pill matches the device's current
                // tier is inert by design (`click_tier` no-ops when you click the
                // tier you are already on — a radio button clicking its own
                // selection). With an unrecognised tier no pill is "current", so
                // every pill click exercises a real change (`set_tier` backend
                // call or the destructive-tier confirm) — which is the control's
                // actual job. The click-the-active-pill no-op is the panel's own
                // unit-tested behaviour, not something a click-walk can assert.
                { "device_id": "d1", "label": "Pixel", "tier": "unset" },
            ]}),
        )
        // Driven by the pairing state below (same reply shape cancel_pairing uses).
        .on(
            "device_gate.start_pairing",
            json!({ "code": "123456", "expires_at": 9_999_999_999.0_f64 }),
        )
        .on("device_gate.cancel_pairing", json!({ "cancelled": true }))
        .on("device_gate.revoke", json!({ "revoked": true }))
        .on("device_gate.set_tier", json!({ "ok": true }))
        .on(
            "device_gate.rotate_token",
            json!({ "new_token": "tok-new" }),
        )
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<DevicesPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = DevicesPanel::new();
        DevicesPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<DevicesPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        // Close every occluding card before each click.
        .reset(|p: &mut DevicesPanel, _w, cx| {
            p.pairing = None;
            p.confirm_revoke = None;
            p.confirm_tier_escalation = None;
            p.rotated = None;
            cx.notify();
        })
        // The pairing card — opened via the real flow so its QR/expiry are real.
        .state("pairing", |p: &mut DevicesPanel, _w, cx| {
            p.start_pairing(cx);
        })
        .state("revoke-confirm", |p: &mut DevicesPanel, _w, cx| {
            p.confirm_revoke = Some("d1".to_string());
            cx.notify();
        })
        .state("tier-escalation-confirm", |p: &mut DevicesPanel, _w, cx| {
            p.confirm_tier_escalation = Some(("d1".to_string(), "read_write".to_string()));
            cx.notify();
        })
        .state("rotated-token", |p: &mut DevicesPanel, _w, cx| {
            p.rotated = Some(RotatedToken {
                device_id: "d1".to_string(),
                new_token: "tok-new".to_string(),
            });
            cx.notify();
        })
        // The empty-list state: `devices-empty-start` paints only when there
        // are no devices — mutually exclusive with the per-device rows, so it
        // needs its own state. Coverage is the union across states.
        .state("empty-list", |p: &mut DevicesPanel, _w, cx| {
            p.devices.clear();
            cx.notify();
        })
        .sources(&[include_str!("../src/devices_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_devices_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_card_states_reach_the_card_only_controls(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    for expected in ["devices-pair-cancel", "devices-rotate-dismiss"] {
        assert!(
            painted.contains(&expected),
            "{expected} paints only inside its card, and the matching state must \
             reach it; got {painted:?}"
        );
    }
}

/// The empty-state "start pairing" control paints only when there are no
/// devices — a separate fixture from the healthy one.
#[gpui::test]
fn the_empty_state_start_control_lives(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on("device_gate.list_devices", json!({ "devices": [] }))
        .on(
            "device_gate.start_pairing",
            json!({ "code": "123456", "expires_at": 9_999_999_999.0_f64 }),
        );
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = ControlWalk::new(window, &fake)
        .fingerprint(fingerprint)
        .reset(|p: &mut DevicesPanel, _w, cx| {
            p.pairing = None;
            cx.notify();
        })
        .sources(&[include_str!("../src/devices_panel.rs")])
        .run(cx);
    let painted = report.painted_ids();
    assert!(
        painted.contains(&"devices-empty-start"),
        "the empty-state start-pairing control paints and works; got {painted:?}"
    );
}

/// A click that drives the panel into the error branch panel_walk never
/// repaints.
#[gpui::test]
fn controls_survive_being_clicked_in_the_error_branch(cx: &mut TestAppContext) {
    let fake =
        ScriptedBackend::new().on_err("device_gate.list_devices", "pipe_unavailable: gate down");
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.error.is_some(), "the fixture really is the error branch");
        })
        .unwrap();

    walk(cx, window, &fake).assert_every_control_lives();
}
