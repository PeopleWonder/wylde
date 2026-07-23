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
        .on(
            "models.resolve_default",
            json!({ "model": "llama3:8b", "source": "default",
                    "stale_default": null, "recommendation": null }),
        );
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
            "models.resolve_default",
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

// ── #235: the persistent default and its fallbacks ───────────────────
//
// Arm 1 stars a row, arm 2 stars nothing and explains itself, arm 3
// renders a recommendation with warnings. The panel reads
// `models.resolve_default` (not the raw `models.get_default`) precisely
// so a star that outlived its model can't light up a row that isn't
// there.

/// Arm 1 — a persisted default that is still installed resolves as the
/// star, and pre-checks its row.
#[gpui::test]
fn models_persisted_default_stars_its_row(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ollama.list_models",
            json!({ "models": [ { "name": "qwen3.5:9b" }, { "name": "llama3.2:3b" } ]}),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on(
            "models.resolve_default",
            json!({ "model": "llama3.2:3b", "source": "default",
                    "stale_default": null, "recommendation": null,
                    "inventory_count": 2 }),
        );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.session_default.as_deref(),
                Some("llama3.2:3b"),
                "the star pre-checks its row — and beats the first entry"
            );
            let r = panel
                .default_resolution
                .as_ref()
                .expect("the resolution landed");
            assert_eq!(r.source, "default");
            assert!(r.stale_default.is_none());
            assert!(r.recommendation.is_none());
        })
        .unwrap();
    assert_eq!(fake.count_for("models.resolve_default"), 1);
}

/// Arm 2 — a default whose model was deleted falls through to
/// first-available. No error, no star on a row that isn't there, and the
/// dangling name is reported so the panel can explain itself.
#[gpui::test]
fn models_deleted_default_falls_through_to_first_available(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ollama.list_models",
            json!({ "models": [ { "name": "qwen3.5:9b" }, { "name": "llama3.2:3b" } ]}),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on(
            "models.resolve_default",
            json!({ "model": "qwen3.5:9b", "source": "first_available",
                    "stale_default": "deepseek-r1:14b", "recommendation": null,
                    "inventory_count": 2 }),
        );
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_none(),
                "a dangling star is an ordinary event, never an error strip"
            );
            assert_eq!(
                panel.session_default, None,
                "first-available is what the picker LANDS on, not a preference \
                 the user expressed — starring it would invent a choice"
            );
            let r = panel
                .default_resolution
                .as_ref()
                .expect("the resolution landed");
            assert_eq!(r.source, "first_available");
            assert_eq!(r.model.as_deref(), Some("qwen3.5:9b"));
            assert_eq!(
                r.stale_default.as_deref(),
                Some("deepseek-r1:14b"),
                "the deleted default is surfaced, not silently dropped"
            );
        })
        .unwrap();
}

/// Arm 3 — an empty store yields the recommend state: qwen3.5:9b, its
/// size, and the warnings, all carried from the harness rather than
/// invented panel-side.
#[gpui::test]
fn models_empty_store_recommends_with_warnings(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on("ollama.list_models", json!({ "models": [] }))
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on(
            "models.resolve_default",
            json!({ "model": null, "source": "recommend",
            "stale_default": null, "inventory_count": 0,
            "recommendation": {
                "model": "qwen3.5:9b",
                "size": "6.6 GB",
                "warnings": [
                    "Download is 6.6 GB.",
                    "Needs roughly 8 GB of VRAM at default context.",
                    "The first message after a pull is slower.",
                    "Nothing is downloaded until you choose to pull it.",
                ],
            }}),
        );
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            // Reachable + genuinely empty is the Empty arm, never
            // Unreachable (#132's distinction still holds).
            assert_eq!(
                classify_installed_section(
                    panel.loading_installed,
                    panel.installed_reachable,
                    panel.installed.is_empty(),
                ),
                InstalledSection::Empty
            );
            let rec = panel
                .default_resolution
                .as_ref()
                .expect("the resolution landed")
                .recommendation
                .as_ref()
                .expect("an empty store carries a recommendation");
            assert_eq!(rec.model, "qwen3.5:9b", "the real ~9B Qwen, not qwen3.6:9b");
            assert_eq!(rec.size, "6.6 GB");
            assert!(
                rec.warnings.len() >= 3,
                "hardware fit, download size and first-run cost all travel with it"
            );
            assert!(
                rec.warnings
                    .iter()
                    .any(|w| w.contains("Nothing is downloaded")),
                "the recommendation is explicitly not an auto-download"
            );
            assert_eq!(
                panel.session_default, None,
                "a recommendation is not a selection — nothing is starred"
            );
        })
        .unwrap();
}

/// A harness that can't be reached must NOT read as "nothing installed"
/// (#132, applied to resolution): the panel keeps no recommendation and
/// invents no default rather than proposing a 6.6 GB download to someone
/// whose models are sitting on disk.
#[gpui::test]
fn models_unreachable_harness_yields_no_recommendation(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ollama.list_models",
            json!({ "models": [ { "name": "qwen3.5:9b" } ]}),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on_err(
            "models.resolve_default",
            "unavailable: the model store is unreachable",
        );
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.default_resolution.is_none(),
                "a failed resolve leaves the prior state alone — it never \
                 fabricates an empty store"
            );
            assert_eq!(panel.session_default, None);
            assert_eq!(
                panel.installed.len(),
                1,
                "the installed list still rendered"
            );
        })
        .unwrap();
}
