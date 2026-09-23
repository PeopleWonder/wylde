//! L7 **control**-walk — Models (issue #247).
//!
//! Models is the most stateful panel in the tree: its thirteen controls live
//! across nine distinct render states — the default header/pull bar, an active
//! search, the pull dialog, a pull in flight, a delete-confirm, two separate
//! HuggingFace strips (a detail strip and a results strip, gated differently),
//! the privacy-gated catalog "Search HF" row, a recommendation card, and the
//! error branch. Each gets a `.state()`; `.reset()` clears the transient ones
//! before every click.
//!
//! Two mechanisms earn their keep here:
//!   * the pull input is armed once at mount (the submit no-ops on empty), and
//!     changing it fires a subscription that clears `hf_selected`, so the HF
//!     states set `pull_selected` to match the query instead of touching the
//!     input; and
//!   * the "Search HF" row is privacy-gated by a process-global pref, seeded
//!     directly via the pipe crate's dev-only `set_cache_for_test` (no disk).

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_pipe::privacy_prefs::{set_cache_for_test, PrivacyPrefs};
use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_models::ipc::{DefaultResolution, PullProgress, Recommended};
use wylde_panel_models::models_panel::{HfSearch, HfSelection, PullState};
use wylde_panel_models::ModelsPanel;

fn fingerprint(p: &ModelsPanel) -> String {
    format!(
        "installed={} loaded={} pull_sel={:?} confirm={:?} hf_sel={:?} err={:?} status={:?} \
         search={:?} pulling={} hf_idle={} default={:?} rec={}",
        p.installed.len(),
        p.loaded.len(),
        p.pull_selected,
        p.confirm_delete,
        p.hf_selected,
        p.error,
        p.status,
        p.search_query,
        p.active_pull.is_some(),
        matches!(p.hf_search, HfSearch::Idle),
        p.session_default,
        p.default_resolution
            .as_ref()
            .and_then(|d| d.recommendation.as_ref())
            .map(|r| r.model.clone())
            .unwrap_or_default(),
    )
}

fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
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
        )
        .on("ollama.delete", json!({ "ok": true }))
        .on("ollama.pull", json!({ "ok": true }))
        .on("hf.search", json!({ "results": [] }))
        .on("models.set_default", json!({ "ok": true }))
}

fn progress() -> PullProgress {
    PullProgress {
        status: "pulling".to_string(),
        completed: 1,
        total: 2,
        digest: String::new(),
    }
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ModelsPanel> {
    // The "Search HF" catalog row only paints with the privacy opt-in on. Seed
    // it once, in-memory (no disk write). Process-global, so the healthy value
    // is fine for the whole test binary.
    set_cache_for_test(PrivacyPrefs {
        hf_search_enabled: true,
        hf_search_warning_shown: true,
    });
    let window = cx.add_window(|_w, cx| {
        let panel = ModelsPanel::new(cx);
        ModelsPanel::spawn_refresh_installed(cx);
        ModelsPanel::spawn_refresh_hardware(cx);
        panel
    });
    cx.run_until_parked();
    // Arm the pull input once (see module docs — must not be re-set per click).
    window
        .update(cx, |p, _w, cx| {
            p.pull_input.update(cx, |i, cx| i.set_text("llama3:8b", cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<ModelsPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .reset(|p: &mut ModelsPanel, _w, cx| {
            p.pull_selected = None;
            p.confirm_delete = None;
            p.hf_selected = None;
            p.active_pull = None;
            p.hf_search = HfSearch::Idle;
            cx.notify();
        })
        // Delete-confirm dialog.
        .state("delete-confirm", |p: &mut ModelsPanel, _w, cx| {
            p.confirm_delete = Some("llama3:8b".to_string());
            cx.notify();
        })
        // A pull in flight — shows the cancel control.
        .state("pull-in-flight", |p: &mut ModelsPanel, _w, cx| {
            p.active_pull = Some(PullState {
                model_name: "llama3:8b".to_string(),
                latest: progress(),
                stream: None,
            });
            cx.notify();
        })
        // HF detail strip (quant picker): query matches pull_selected so the
        // panel is not "searching", and a result is staged. The input is
        // already "llama3:8b" from mount — do NOT set it here, or the on-change
        // subscription fires and clears the `hf_selected` we are about to set.
        .state("hf-detail", |p: &mut ModelsPanel, _w, cx| {
            p.pull_selected = Some("llama3:8b".to_string());
            p.hf_selected = Some(HfSelection {
                repo_id: "TheBloke/Llama-2-7B-GGUF".to_string(),
                quant: "Q4_K_M".to_string(),
            });
            cx.notify();
        })
        // HF results strip (its own close control): the online search is active.
        .state("hf-results", |p: &mut ModelsPanel, _w, cx| {
            p.hf_search = HfSearch::Empty;
            cx.notify();
        })
        // The recommendation card (its "pull recommended" button) renders in
        // the empty-installed state, so clear the installed list too.
        .state("recommendation", |p: &mut ModelsPanel, _w, cx| {
            p.installed.clear();
            p.default_resolution = Some(DefaultResolution {
                model: None,
                source: "recommend".to_string(),
                stale_default: None,
                recommendation: Some(Recommended {
                    model: "qwen2.5:0.5b".to_string(),
                    size: "400 MB".to_string(),
                    warnings: vec![],
                }),
            });
            cx.notify();
        })
        // The installed-model search box, populated — shows the clear control.
        .state("search-active", |p: &mut ModelsPanel, _w, cx| {
            p.search_input.update(cx, |i, cx| i.set_text("lla", cx));
            cx.notify();
        })
        // The installed section unreachable — shows the retry control.
        .state("installed-unreachable", |p: &mut ModelsPanel, _w, cx| {
            p.installed_reachable = false;
            p.installed.clear();
            cx.notify();
        })
        // MUST be last: this is the only state that changes the pull INPUT, and
        // `reset` does not restore it (a re-set would fire the on-change
        // subscription that clears `hf_selected`). A state that changed the
        // input earlier would leave every later state seeing the wrong query —
        // the hf-detail strip in particular vanishes once the query stops
        // matching `pull_selected`. Keeping it last means the earlier states run
        // against the clean "llama3:8b" the mount armed.
        //
        // The catalog dropdown for an UNKNOWN query shows "pull anyway" and,
        // with the privacy opt-in, the "Search HF" row.
        .state("catalog-unknown-query", |p: &mut ModelsPanel, _w, cx| {
            p.pull_input
                .update(cx, |i, cx| i.set_text("totally-unknown-xyz", cx));
            cx.notify();
        })
        .sources(&[include_str!("../src/models_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_models_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

/// The error branch — `panel_walk` cannot repaint it after a click, and the
/// retry control is the user's one way out.
#[gpui::test]
fn controls_survive_being_clicked_in_the_error_branch(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("ollama.list_models", "pipe_unavailable: ollama down")
        .on_err("system.inventory", "pipe_unavailable: broker down")
        .on_err("ollama.list_loaded", "pipe_unavailable: ollama down")
        .on_err("models.resolve_default", "pipe_unavailable: harness down");
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.error.is_some(), "the fixture really is the error branch");
        })
        .unwrap();

    ControlWalk::new(window, &fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/models_panel.rs")])
        .run(cx)
        .assert_every_control_lives();
}
