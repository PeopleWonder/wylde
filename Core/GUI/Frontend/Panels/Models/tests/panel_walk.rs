//! L7 panel-walk — Models (issue #35, roadmap T0.1b).
//!
//! One of the four zero-coverage panels. Mount the real `ModelsPanel` the way
//! the Shell does (`new(cx)` + the four `spawn_*` loaders `view` kicks off),
//! drive the Ollama/broker/harness IPC through the scripted fake, and assert
//! it survives every realistic backend condition.
//!
//! **What "error state" means for Models:** a panel-level `error: Option<String>`
//! set when `ollama.list_models` (or a pull/delete/set-default) fails, plus
//! `loading_installed` / `loading_hardware` flags that must clear once their
//! first reply lands. The HuggingFace online search has its own `hf_search`
//! channel (`Failed(_)`), untouched at mount. The gate: `error.is_none()` +
//! flags cleared on the happy path; `error.is_some()` when Ollama is down.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_models::ModelsPanel;

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ModelsPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = ModelsPanel::new(cx);
        ModelsPanel::spawn_refresh_installed(cx);
        ModelsPanel::spawn_refresh_hardware(cx);
        ModelsPanel::spawn_loaded_poll(cx);
        ModelsPanel::spawn_load_default(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn models_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ollama.list_models",
            json!({ "models": [
                { "name": "llama3:8b", "family": "llama", "parameter_size": "8B" },
            ]}),
        )
        .on("system.inventory", json!({ "cpu_brand": "Test CPU", "cpu_cores": 8 }))
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on("models.get_default", json!({ "model": "llama3:8b" }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(!panel.loading_installed, "the installed-list spinner cleared");
            assert!(!panel.loading_hardware, "the hardware spinner cleared");
            assert_eq!(panel.installed.len(), 1, "the installed model loaded");
        })
        .unwrap();
    assert_eq!(fake.count_for("ollama.list_models"), 1);
}

#[gpui::test]
fn models_survives_backend_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("ollama.list_models", "pipe_unavailable: ollama not running")
        .on_err("system.inventory", "pipe_unavailable: vram-broker not running")
        .on_err("ollama.list_loaded", "pipe_unavailable: ollama not running")
        .on_err("models.get_default", "pipe_unavailable: harness not running");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "a down Ollama surfaces a visible error, not a silent empty grid"
            );
            assert!(!panel.loading_installed, "no stuck spinner on failure");
            assert!(!panel.loading_hardware);
        })
        .unwrap();
}

#[gpui::test]
fn models_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err("ollama.list_models", "internal_error: ollama blew up");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_some(), "an error envelope surfaces on the panel");
            assert!(!panel.loading_installed);
        })
        .unwrap();
}

#[gpui::test]
fn models_tolerates_empty_backend(cx: &mut TestAppContext) {
    // Default fake → Ok({}); `ollama.list_models` parses to an empty grid.
    // Empty is the "no models pulled yet" state, not an error.
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "empty ok-replies are not an error");
            assert!(!panel.loading_installed);
            assert!(panel.installed.is_empty(), "no installed models is a clean empty state");
        })
        .unwrap();
}
