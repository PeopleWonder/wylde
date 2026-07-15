//! L7 panel-walk — Settings (issue #35, roadmap T0.1b).
//!
//! Settings already has behavioural coverage (`prefs_dispatch.rs`); this is the
//! uniform mount-under-every-backend-condition smoke. Mount the real
//! `SettingsPanel` the way the Shell does (`new()` + `init_ollama_inputs` +
//! `init_profile_inputs` + `spawn_refresh`).
//!
//! **What "error state" means for Settings:** every section on
//! `spawn_refresh` is best-effort — a down service degrades that section to
//! defaults rather than raising a page-level banner, so the panel-level
//! `error: Option<String>` stays `None` at mount by design (it's reserved for
//! failed *writes*). Per-section error detection is a flag: the optional voice
//! service flips `voice_offline`. Gate: `error.is_none()` always at mount;
//! `voice_offline` tracks the voice service (false healthy, true down).
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_settings::SettingsPanel;

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

#[gpui::test]
fn settings_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on("voice.get_config", json!({ "wake_word_enabled": true }))
        .on("consent.list", json!({ "entries": [] }));
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no page-level error at mount");
            assert!(!panel.voice_offline, "the voice section is online when it answers");
        })
        .unwrap();
}

#[gpui::test]
fn settings_survives_backend_down(cx: &mut TestAppContext) {
    // Every read fails. Settings must degrade each section to defaults — NOT a
    // page banner — and the optional voice section flags itself offline.
    let fake = ScriptedBackend::new()
        .on_err("voice.get_config", "pipe_unavailable: voice not running")
        .on_err("consent.list", "pipe_unavailable: harness not running")
        .on_err("models.get_effective", "pipe_unavailable: harness not running");
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_none(),
                "a down service degrades sections to defaults, not a page-level banner"
            );
            assert!(
                panel.voice_offline,
                "the optional voice section DETECTS the service is down"
            );
        })
        .unwrap();
}

#[gpui::test]
fn settings_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err("voice.get_config", "internal_error: voice blew up");
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "still no page-level banner on an error envelope");
            assert!(panel.voice_offline, "the voice section reads offline on an error envelope");
        })
        .unwrap();
}

#[gpui::test]
fn settings_tolerates_empty_backend(cx: &mut TestAppContext) {
    // Default fake → Ok({}); every read parses to defaults. The voice config
    // defaults are a valid "online-with-defaults" state, not offline.
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "empty ok-replies are not an error");
            assert!(!panel.voice_offline, "an empty voice envelope parses to online defaults");
        })
        .unwrap();
}
