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
use wylde_panel_settings::ipc::UpdateCheck;
use wylde_panel_settings::SettingsPanel;
use wylde_updater::{ReleaseAsset, StackAsset, UpdateInfo};

/// One resolved stack member: the binary plus its own `.minisig` sibling,
/// the pair `pick_assets` produces for every roster entry.
fn stack_asset(name: &str, image: &str) -> StackAsset {
    StackAsset {
        name: name.into(),
        image: image.into(),
        binary: ReleaseAsset {
            name: image.into(),
            url: format!("https://example.test/{image}"),
            size: 1,
        },
        signature: ReleaseAsset {
            name: format!("{image}.minisig"),
            url: format!("https://example.test/{image}.minisig"),
            size: 1,
        },
    }
}

/// An `UpdateCheck::Available` seeded with the given version, for the
/// skip-version windowed test. Since #97 an update carries the whole stack,
/// so the fixture carries more than the shell; the panel only reads the
/// version and the notes, but the shape must match what a real check
/// resolves.
fn available(version: &str) -> UpdateCheck {
    UpdateCheck::Available(UpdateInfo {
        version: version.into(),
        tag: format!("v{version}"),
        notes: "- fixed a thing\n- added another".into(),
        html_url: "https://example.test/r".into(),
        assets: vec![
            stack_asset("wylde-gui", "wylde-gui.exe"),
            stack_asset("wylde-lifecycle", "wylde-lifecycle.exe"),
        ],
    })
}

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
fn switching_to_experimental_warns_before_persisting(cx: &mut TestAppContext) {
    // req 6: stable → experimental opens the warning and persists NOTHING;
    // the switch waits on confirm.
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(false, "beta", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    window.update(cx, |p, _w, cx| p.cycle_channel(cx)).unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.channel_warning_open, "the experimental warning is up");
            assert_eq!(
                p.update_prefs.channel, "stable",
                "channel is not yet switched"
            );
        })
        .unwrap();
    assert!(
        fake.last_call_for("updater.set_prefs").is_none(),
        "no write happens until the warning is acknowledged"
    );

    // Confirm → the switch persists channel=beta and closes the modal.
    window
        .update(cx, |p, _w, cx| p.confirm_channel_warning(cx))
        .unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("confirming the warning persists the switch");
    assert_eq!(call.payload_str("channel").as_deref(), Some("beta"));
    window
        .update(cx, |p, _w, _cx| assert!(!p.channel_warning_open))
        .unwrap();
}

#[gpui::test]
fn cancelling_the_experimental_warning_stays_on_stable(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(false, "beta", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    window.update(cx, |p, _w, cx| p.cycle_channel(cx)).unwrap();
    window
        .update(cx, |p, _w, cx| p.cancel_channel_warning(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            assert!(!p.channel_warning_open, "the warning closed");
            assert_eq!(p.update_prefs.channel, "stable", "still on stable");
        })
        .unwrap();
    assert!(
        fake.last_call_for("updater.set_prefs").is_none(),
        "cancelling persists nothing"
    );
}

#[gpui::test]
fn switching_back_to_stable_is_immediate(cx: &mut TestAppContext) {
    // beta → stable is un-gated: no warning, persists straight away.
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(false, "stable", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, _cx| p.update_prefs.channel = "beta".into())
        .unwrap();
    window.update(cx, |p, _w, cx| p.cycle_channel(cx)).unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            assert!(!p.channel_warning_open, "no warning on the way to stable");
        })
        .unwrap();
    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("the switch to stable persists immediately");
    assert_eq!(call.payload_str("channel").as_deref(), Some("stable"));
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
        .update(cx, |p, _w, cx| p.toggle_updates_enabled(cx))
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

#[gpui::test]
fn enabling_auto_check_gates_on_the_consent_modal(cx: &mut TestAppContext) {
    // req 4: turning auto-check ON opens the consent modal and persists
    // NOTHING; the enable waits on confirm.
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(true, "stable", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, cx| p.toggle_auto_check(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.auto_check_modal_open, "the consent modal is up");
            assert!(!p.update_prefs.auto_check, "auto-check is not yet armed");
        })
        .unwrap();
    assert!(
        fake.last_call_for("updater.set_prefs").is_none(),
        "no network-arming write until the user consents"
    );

    // Confirm → auto-check persists true and the modal closes.
    window
        .update(cx, |p, _w, cx| p.confirm_auto_check_modal(cx))
        .unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("confirming persists auto_check");
    assert_eq!(
        call.payload.get("auto_check").and_then(|v| v.as_bool()),
        Some(true)
    );
    window
        .update(cx, |p, _w, _cx| {
            assert!(!p.auto_check_modal_open);
            assert!(p.update_prefs.auto_check);
        })
        .unwrap();
}

#[gpui::test]
fn disabling_auto_check_is_immediate(cx: &mut TestAppContext) {
    // Turning auto-check OFF needs no disclosure (becoming more isolated).
    let fake = ScriptedBackend::new().on("updater.set_prefs", merged(true, "stable", "weekly"));
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, _cx| p.update_prefs.auto_check = true)
        .unwrap();
    window
        .update(cx, |p, _w, cx| p.toggle_auto_check(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            assert!(!p.auto_check_modal_open, "no modal on the way off");
        })
        .unwrap();
    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("disabling persists immediately");
    assert_eq!(
        call.payload.get("auto_check").and_then(|v| v.as_bool()),
        Some(false)
    );
}

#[gpui::test]
fn decline_skips_the_version_and_clears_the_card(cx: &mut TestAppContext) {
    // req 7: "Decline (Skip this version)" persists the version and returns
    // the section to up-to-date so the changelog card dismisses.
    let reply = json!({
        "enabled": true, "auto_check": true, "frequency": "weekly",
        "channel": "stable", "last_checked": 0, "skipped_version": "9.9.9",
    });
    let fake = ScriptedBackend::new().on("updater.set_prefs", reply);
    let _guard = fake.clone().install();
    let window = mount(cx);

    // Seed an available update and let it render — this draws the changelog
    // card (release notes + Accept / Decline), proving that surface mounts.
    window
        .update(cx, |p, _w, cx| {
            p.update_check = available("9.9.9");
            cx.notify();
        })
        .unwrap();
    cx.run_until_parked();

    // Now decline it.
    window.update(cx, |p, _w, cx| p.skip_version(cx)).unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("updater.set_prefs")
        .expect("declining persists the skipped version");
    assert_eq!(
        call.payload_str("skipped_version").as_deref(),
        Some("9.9.9")
    );
    window
        .update(cx, |p, _w, _cx| {
            assert!(
                matches!(p.update_check, UpdateCheck::UpToDate),
                "the card is cleared to up-to-date after skipping"
            );
            assert_eq!(
                p.update_prefs.skipped_version.as_deref(),
                Some("9.9.9"),
                "local prefs mirror the skip"
            );
        })
        .unwrap();
}
