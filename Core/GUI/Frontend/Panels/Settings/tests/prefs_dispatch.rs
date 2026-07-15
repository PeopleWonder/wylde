//! Windowed gpui tests for the Settings → Updates controls (GUI-responsiveness
//! pass). The headline fix of that pass: every Updates toggle dispatched
//! `updater.set_prefs` against the lifecycle daemon, which registered no such
//! verb — so every toggle errored. These tests pin that the toggles dispatch
//! the right verb+patch to the lifecycle daemon and that a failure surfaces.
//!
//! Mount a real SettingsPanel and drive its toggle methods through the scripted
//! fake backend at the `wylde_gui_pipe::call` seam — no live lifecycle daemon.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_settings::SettingsPanel;

/// A merged-prefs reply shaped like the (now-registered) handler returns.
fn merged(enabled: bool, channel: &str, frequency: &str) -> serde_json::Value {
    json!({
        "enabled": enabled, "auto_check": true, "frequency": frequency,
        "channel": channel, "last_checked": 0,
    })
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<SettingsPanel> {
    let window = cx.add_window(|_w, _cx| SettingsPanel::new());
    cx.run_until_parked();
    window
}

#[gpui::test]
fn toggle_updates_enabled_persists_the_flipped_flag(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(true, "stable", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    let before = window
        .update(cx, |p, _w, _cx| p.update_prefs.enabled)
        .unwrap();
    window
        .update(cx, |p, _w, cx| p.toggle_updates_enabled(cx))
        .unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("the master toggle must persist via updater.set_prefs");
    assert_eq!(
        call.service, "wylde-lifecycle",
        "updater prefs live on the lifecycle daemon (the verb the fix registered there)"
    );
    assert_eq!(
        call.payload.get("enabled").and_then(|v| v.as_bool()),
        Some(!before),
        "the patch carries exactly the flipped flag"
    );
    window
        .update(cx, |p, _w, _cx| {
            assert_eq!(
                p.update_prefs.enabled, !before,
                "the merged reply is adopted"
            );
            assert!(p.error.is_none(), "a registered verb means no error banner");
        })
        .unwrap();
}

#[gpui::test]
fn cycle_channel_persists_the_channel_patch(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(false, "beta", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    // Default channel is stable; one cycle → beta.
    window.update(cx, |p, _w, cx| p.cycle_channel(cx)).unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("channel cycle persists");
    assert_eq!(
        call.payload_str("channel").as_deref(),
        Some("beta"),
        "the patch carries the next channel"
    );
}

#[gpui::test]
fn a_prefs_write_failure_surfaces_in_the_error_banner(cx: &mut TestAppContext) {
    // The pre-fix symptom: the verb wasn't registered → no_action. The panel
    // must surface that, not swallow it.
    let fake = ScriptedBackend::new().on_err(
        "updater.set_prefs",
        "no_action: unknown action updater.set_prefs",
    );
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, cx| p.toggle_auto_check(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            let e = p.error.as_deref().unwrap_or_default();
            assert!(
                e.contains("update prefs"),
                "a failed prefs write is surfaced: {e:?}"
            );
        })
        .unwrap();
}
