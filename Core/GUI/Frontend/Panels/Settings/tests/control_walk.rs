//! L7 **control**-walk — Settings (issue #247).
//!
//! Harness: `wylde_gui_test_support::control_walk`. Settings is the panel that
//! most exercises the walk's **named states** (three modal-gated controls) and
//! its **reset** hook (those modals are occluding backdrops).

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_settings::SettingsPanel;

fn fingerprint(p: &SettingsPanel) -> String {
    // Wide on purpose: a control's effect is only "observable" if the field it
    // moves is in here. `hf_dont_show_again` earns its place because the modal
    // checkbox toggles exactly that and nothing else — omit it and a live
    // control reads as dead.
    format!(
        "autostart={} autostart_err={:?} hf_modal={} auto_modal={} chan_modal={} \
         model={:?} voice={:?} privacy_hf_warn={} dont_show={} capturing={} note={:?} \
         err={:?} consent={:?}",
        p.autostart_enabled,
        p.autostart_error,
        p.hf_modal_open,
        p.auto_check_modal_open,
        p.channel_warning_open,
        p.ollama_model,
        p.voice,
        p.privacy.hf_search_warning_shown,
        p.hf_dont_show_again,
        p.capturing_hotkey,
        p.hotkey_note,
        p.error,
        p.consent,
    )
}

fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on("voice.get_config", json!({ "wake_word_enabled": true }))
        // A per-tool decision, so the per-tool row and the "reset consent"
        // button both paint — an empty `tools` map renders neither.
        .on(
            "consent.list",
            json!({ "no_auth": false, "tools": { "search": "approved" } }),
        )
        // The per-tool row cycles a decision; the reset clears them all.
        .on(
            "consent.set",
            json!({ "no_auth": false, "tools": { "search": "denied" } }),
        )
        .on("consent.clear", json!({ "no_auth": false, "tools": {} }))
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<SettingsPanel> {
    let window = cx.add_window(|_w, cx| {
        let mut panel = SettingsPanel::new();
        panel.init_ollama_inputs(cx);
        panel.init_profile_inputs(cx);
        SettingsPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<SettingsPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        // Close every modal before each click — they are occluding backdrops,
        // so one left open would swallow every subsequent click in the pass.
        //
        // Also arm the precondition the privacy-reset button needs: it clears
        // `hf_search_warning_shown`, which is a no-op (and so looks dead) unless
        // the flag is set first. Establishing it here means the reset button
        // always has something to reset, whatever order the walk reaches it in.
        .reset(|p: &mut SettingsPanel, _w, cx| {
            p.hf_modal_open = false;
            p.auto_check_modal_open = false;
            p.channel_warning_open = false;
            p.privacy.hf_search_warning_shown = true;
            cx.notify();
        })
        // One state per modal so the modal-only controls paint and get walked.
        .state("hf-modal-open", |p: &mut SettingsPanel, _w, cx| {
            p.hf_modal_open = true;
            cx.notify();
        })
        .state("auto-check-modal-open", |p: &mut SettingsPanel, _w, cx| {
            p.auto_check_modal_open = true;
            cx.notify();
        })
        .state("channel-warning-open", |p: &mut SettingsPanel, _w, cx| {
            p.channel_warning_open = true;
            cx.notify();
        })
        .sources(&[include_str!("../src/sections.rs")])
        .run(cx)
}

#[gpui::test]
fn every_settings_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_modal_states_reach_the_modal_only_controls(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    assert!(
        painted.contains(&"settings-hf-modal-dontshow"),
        "the HF modal's checkbox paints only while that modal is open, and the \
         `hf-modal-open` state must reach it; got {painted:?}"
    );
}
