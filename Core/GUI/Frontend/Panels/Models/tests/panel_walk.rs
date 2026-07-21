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
use wylde_panel_models::models_panel::{
    classify_installed_section, is_unreferenced, slot_role, InstalledSection, SlotRole,
};
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
        .on(
            "system.inventory",
            json!({ "cpu_brand": "Test CPU", "cpu_cores": 8 }),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on("models.get_default", json!({ "model": "llama3:8b" }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(
                !panel.loading_installed,
                "the installed-list spinner cleared"
            );
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
        .on_err(
            "system.inventory",
            "pipe_unavailable: vram-broker not running",
        )
        .on_err("ollama.list_loaded", "pipe_unavailable: ollama not running")
        .on_err(
            "models.get_default",
            "pipe_unavailable: harness not running",
        );
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
    let fake =
        ScriptedBackend::new().on_err("ollama.list_models", "internal_error: ollama blew up");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "an error envelope surfaces on the panel"
            );
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
            assert!(
                panel.installed.is_empty(),
                "no installed models is a clean empty state"
            );
            // The store answered — a genuinely empty store, not unreachable.
            assert!(panel.installed_reachable);
            assert_eq!(
                classify_installed_section(
                    panel.loading_installed,
                    panel.installed_reachable,
                    panel.installed.is_empty(),
                ),
                InstalledSection::Empty,
                "a reachable empty store is the 'pull your first model' state"
            );
        })
        .unwrap();
}

/// #132: an empty list caused by an UNREACHABLE store (e.g. the daemon is
/// still restarting right after an update) must NOT render as the "you have
/// no models, pull one" empty state — that would tell a user with a full
/// disk that their models are gone. It classifies as `Unreachable` instead.
#[gpui::test]
fn models_unreachable_store_is_not_an_empty_store(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err("ollama.list_models", "pipe_unavailable: ollama down");
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.installed.is_empty(), "nothing was listed");
            assert!(
                !panel.installed_reachable,
                "the store was flagged unreachable, not empty"
            );
            assert_eq!(
                classify_installed_section(
                    panel.loading_installed,
                    panel.installed_reachable,
                    panel.installed.is_empty(),
                ),
                InstalledSection::Unreachable,
                "an unreachable store must not read as an empty store (#132)"
            );
        })
        .unwrap();
}

/// #131: a completed delete reports the bytes it freed. The wrapper returns
/// `freed_bytes`; the panel surfaces a "Freed …" success line.
#[gpui::test]
fn models_reports_bytes_freed_on_delete(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ollama.list_models",
            json!({ "models": [ { "name": "qwen2.5:1.5b", "size": 1_500_000_000_u64 } ]}),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on(
            "ollama.delete",
            json!({ "ok": true, "freed": true, "freed_bytes": 1_500_000_000_u64 }),
        );
    let _guard = fake.install();

    let window = mount(cx);

    // Stage the confirmation then commit it.
    window
        .update(cx, |panel, _w, cx| {
            assert_eq!(panel.installed.len(), 1, "the model to delete is listed");
            panel.request_delete("qwen2.5:1.5b".to_owned(), cx);
            panel.confirm_delete(cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            let status = panel.status.as_deref().unwrap_or_default();
            assert!(
                status.contains("Freed"),
                "delete surfaced a bytes-freed line, got {status:?}"
            );
            assert!(
                status.contains("1.4 GB"),
                "it reports the freed size, got {status:?}"
            );
        })
        .unwrap();
}

/// #131: each installed model is labelled with what the running config
/// references it as, so "safe to delete?" is answerable at a glance. A slot
/// model reads as its slot (across `:latest` normalisation); a hand-pulled
/// model reads as unreferenced (safe to delete).
#[gpui::test]
fn models_label_reference_slots(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ollama.list_models",
            json!({ "models": [
                { "name": "qwen2.5:7b" },
                { "name": "nomic-embed-text:latest" },
                { "name": "mistral:7b" },
            ]}),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on(
            "settings.reasoning.get",
            json!({ "slots": {
                "reasoner": "qwen2.5:7b",
                "fast": "qwen2.5:7b",
                "embedder": "nomic-embed-text",
            }}),
        );
    let _guard = fake.install();

    let window = mount(cx);
    // `mount` doesn't kick the reference refresh (it mirrors the four
    // loaders the Shell runs); the real `view()` does. Drive it here.
    window
        .update(cx, |_panel, _w, cx| {
            ModelsPanel::spawn_refresh_references(cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.references.reasoner, "qwen2.5:7b");
            assert_eq!(
                slot_role("qwen2.5:7b", &panel.references),
                Some(SlotRole::Reasoner)
            );
            // The embedder slot stores the bare tag; the row reports the
            // implicit `:latest` — normalisation still matches.
            assert_eq!(
                slot_role("nomic-embed-text:latest", &panel.references),
                Some(SlotRole::Embedder)
            );
            // The hand-pulled model fills no slot, isn't default, isn't
            // loaded ⇒ safe to delete.
            assert!(slot_role("mistral:7b", &panel.references).is_none());
            assert!(is_unreferenced(
                "mistral:7b",
                &panel.references,
                false,
                false
            ));
        })
        .unwrap();
}
