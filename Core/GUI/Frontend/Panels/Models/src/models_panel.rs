//! Models panel View — install / pull / delete / set-default surface
//! over the local Ollama daemon.
//!
//! State:
//!   * `installed`             — last `ollama.list_models` reply.
//!   * `loaded`                — `ollama.list_loaded` names; sources
//!     the "in use" pill.
//!   * `pull_input`            — `Entity<TextInput>`; "name to pull".
//!   * `active_pull`           — `Some(PullState)` while a stream is
//!     open.  Dropping the stream cancels the upstream pull (the
//!     harness wires client-disconnect → abandon).
//!   * `confirm_delete`        — the row id currently asking for an
//!     inline "Confirm delete?" Yes/Cancel.  Mutually exclusive — only
//!     one row at a time.
//!   * `session_default`       — per-session preference for the
//!     "default model" star.  Persistent storage waits on a
//!     `models.set_default` pipe verb the harness doesn't ship yet.
//!   * `hardware`              — `system.inventory` snapshot used by
//!     `recommend::pick`.  Empty / unknown is fine.
//!   * `error`                 — last surfaced pipe error.
//!   * `loading_*`             — per-section busy flags.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, IntoElement, Render, SharedString, Stateful, Subscription, Window,
};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::catalog::{self, CatalogEntry};
use crate::hf::{self, HfModel};
use crate::ipc::{
    delete_installed_model, list_installed_models, list_loaded_model_names, pull_model,
    read_hardware, DefaultResolution, HardwareSnapshot, InstalledModel, PullProgress, Recommended,
    ReferenceSet,
};
use crate::recommend::{pick as pick_recommendations, Recommendation};
use wylde_gui_controls::control;

/// Max catalog suggestions shown in the autocomplete dropdown.
const CATALOG_SUGGESTION_LIMIT: usize = 10;

/// How often the panel re-polls `ollama.list_loaded` so the "in use"
/// pill tracks what the broker is actually holding.  Same 5 s cadence
/// the Svelte Models page uses.
const LOADED_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct ModelsPanel {
    pub installed: Vec<InstalledModel>,
    pub loaded: Vec<String>,
    pub pull_input: Entity<TextInput>,
    /// Mirror of `pull_input`'s text, updated on every `Changed`.  Drives
    /// the catalog autocomplete dropdown: a plain field read in render
    /// rather than reaching into the input handle per frame.
    pub pull_query: String,
    /// The tag last chosen from the autocomplete dropdown.  While the
    /// live query still equals it the dropdown stays closed (the user is
    /// looking at their pick, not searching); typing anything else
    /// diverges the query and re-opens the suggestions.  Selecting a row
    /// calls `set_text`, which re-emits `Changed` — comparing against
    /// this latch is what keeps that event from re-opening the dropdown.
    pub pull_selected: Option<String>,
    /// Single-line input owning the live filter query.  Pure
    /// presentation — the backend always serves the full installed list;
    /// this box only narrows what the View renders.  Empty means "show
    /// everything".
    pub search_input: Entity<TextInput>,
    /// Mirror of `search_input`'s text, updated on every `Changed` so the
    /// filter is a plain field read in render (no reaching into the input
    /// handle per row).
    pub search_query: String,
    pub active_pull: Option<PullState>,
    pub confirm_delete: Option<String>,
    pub session_default: Option<String>,
    /// The #235 resolution the panel last read from
    /// `models.resolve_default` — the star checked against the live
    /// inventory, with its fallbacks. `None` until the first reply lands
    /// (or if the harness is down; a failure deliberately leaves the
    /// prior value rather than reading as "nothing installed").
    ///
    /// Drives two things the raw star can't: the *recommend card* on an
    /// empty store, and the "your default was deleted" note when a star
    /// falls through.
    pub default_resolution: Option<DefaultResolution>,
    pub hardware: HardwareSnapshot,
    /// State of an opt-in HuggingFace online search (privacy-gated). Stays
    /// `Idle` unless the user explicitly triggers a search.
    pub hf_search: HfSearch,
    /// A HuggingFace result the user picked + its chosen quant. Drives the
    /// detail strip; the resolved `hf.co/...` tag also lives in the pull
    /// input so the existing Pull button commits it.
    pub hf_selected: Option<HfSelection>,
    /// The term the active/last online search ran on — header + empty-state
    /// copy read it.
    pub hf_query: String,
    pub error: Option<String>,
    /// Whether the LAST `ollama.list_models` attempt reached the daemon.
    /// Drives the #132 distinction between a genuinely empty store and one
    /// that was merely unreachable (so an empty list never reads as "you
    /// have no models" when the models are safe on disk). Starts `true` so
    /// the initial spinner doesn't flash the unreachable state.
    pub installed_reachable: bool,
    /// Transient success line (e.g. "Freed 1.4 GB — deleted qwen2.5:1.5b")
    /// shown after a completed delete; cleared when the next mutating
    /// action starts.
    pub status: Option<String>,
    /// The reasoning slots the running config references, from
    /// `settings.reasoning.get`. Empty until the first reply lands (and on
    /// a down harness); drives the per-row slot / "not referenced" labels.
    pub references: ReferenceSet,
    pub loading_installed: bool,
    pub loading_hardware: bool,
    _input_sub: Subscription,
    _search_sub: Subscription,
}

/// In-flight pull bookkeeping.  The `stream` field holds the
/// `PipeStream`; dropping it cancels the upstream pull.
pub struct PullState {
    pub model_name: String,
    pub latest: PullProgress,
    pub stream: Option<wylde_gui_pipe::PipeStream>,
}

/// State of an opt-in HuggingFace online search (privacy-gated). Only ever
/// leaves `Idle` after the user clicks the "Search HuggingFace" affordance,
/// which itself only appears when the Settings toggle is on. Mirrors the
/// Settings panel's `UpdateCheck` shape.
#[derive(Debug, Clone, Default)]
pub enum HfSearch {
    /// No online search active — the panel shows its normal catalog flow.
    #[default]
    Idle,
    /// A query is in flight.
    Searching,
    /// Results came back (non-empty).
    Results(Vec<HfModel>),
    /// The query succeeded but matched nothing.
    Empty,
    /// The query failed (offline, rate-limited, timeout); carries the
    /// message to surface inline.
    Failed(String),
}

/// A chosen HuggingFace result + the quant the user picked for it. Drives
/// the detail strip and the `hf.co/<repo>:<quant>` tag dropped into the
/// pull field.
#[derive(Debug, Clone, PartialEq)]
pub struct HfSelection {
    pub repo_id: String,
    pub quant: String,
}

impl ModelsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let pull_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Search models to pull (e.g. llama, qwen 7b, coder)…")
                .with_submit_mode(SubmitMode::EnterSubmits)
                .with_min_height(32.0)
                .with_element_key("models-pull-input")
        });
        let pull_input_for_sub = pull_input.clone();
        let input_sub = cx.subscribe(
            &pull_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                match event {
                    // Live keystrokes feed the autocomplete.  Mirror the
                    // text so render reads a plain field.
                    InputEvent::Changed(text) => {
                        this.pull_query = text.clone();
                        // Editing past a selected HF tag returns to the
                        // catalog flow and drops any stale online results.
                        // Selecting a result / cycling its quant sets the
                        // field to exactly that tag, so those self-emitted
                        // `Changed` events match and are preserved.
                        let on_hf_tag = this
                            .hf_selected
                            .as_ref()
                            .map(|s| hf::to_pull_tag(&s.repo_id, &s.quant) == *text)
                            .unwrap_or(false);
                        if !on_hf_tag {
                            this.hf_selected = None;
                            this.hf_search = HfSearch::Idle;
                        }
                        cx.notify();
                    }
                    // Enter pulls exactly what's typed — works for a
                    // catalogued tag or an uncatalogued one alike.
                    InputEvent::Submit(_text) => {
                        let name = pull_input_for_sub.read(cx).text().trim().to_owned();
                        if !name.is_empty() {
                            pull_input_for_sub.update(cx, |i, cx| i.clear(cx));
                            this.pull_query.clear();
                            this.pull_selected = None;
                            this.start_pull(name, cx);
                        }
                    }
                }
            },
        );

        let search_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Search models…")
                .with_submit_mode(SubmitMode::Never)
                .with_min_height(32.0)
                .with_element_key("models-search")
        });
        let search_sub = cx.subscribe(
            &search_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::Changed(text) = event {
                    this.search_query = text.clone();
                    cx.notify();
                }
            },
        );

        Self {
            installed: Vec::new(),
            loaded: Vec::new(),
            pull_input,
            pull_query: String::new(),
            pull_selected: None,
            search_input,
            search_query: String::new(),
            active_pull: None,
            confirm_delete: None,
            session_default: None,
            default_resolution: None,
            hardware: HardwareSnapshot::default(),
            hf_search: HfSearch::Idle,
            hf_selected: None,
            hf_query: String::new(),
            error: None,
            installed_reachable: true,
            status: None,
            references: ReferenceSet::default(),
            loading_installed: true,
            loading_hardware: true,
            _input_sub: input_sub,
            _search_sub: search_sub,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            Self::spawn_refresh_installed(cx);
            Self::spawn_refresh_hardware(cx);
            Self::spawn_loaded_poll(cx);
            Self::spawn_load_default(cx);
            Self::spawn_refresh_references(cx);
            panel
        })
        .into()
    }

    pub fn spawn_refresh_installed(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_installed_models().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(rows) => {
                        panel.error = None;
                        panel.installed = rows;
                        panel.installed_reachable = true;
                    }
                    Err(err) => {
                        panel.error = Some(err);
                        // The store was unreachable — DON'T blank the list
                        // (a stale list is better than a false empty) and
                        // flag it so an empty list renders the "safe on
                        // disk" state, not "no models pulled yet" (#132).
                        panel.installed_reachable = false;
                    }
                }
                panel.loading_installed = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Refresh the reasoning-slot reference set so each installed row can
    /// show whether the running config references it. Soft-fails: a down
    /// harness leaves `references` empty (no slot labels) rather than
    /// surfacing an error.
    pub fn spawn_refresh_references(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = crate::ipc::get_reasoning_slots().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(refs) = outcome {
                    panel.references = refs;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn spawn_refresh_hardware(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = read_hardware().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(hw) = outcome {
                    panel.hardware = hw;
                }
                panel.loading_hardware = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Long-lived task — every `LOADED_POLL_INTERVAL` re-reads
    /// `ollama.list_loaded` and merges the names into `loaded` so the
    /// per-row "in use" pill stays in sync with what the daemon
    /// actually has resident.
    pub fn spawn_loaded_poll(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                let outcome = list_loaded_model_names().await;
                let still_alive = this
                    .update(app_cx, |panel, cx| {
                        if let Ok(names) = outcome {
                            panel.loaded = names;
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !still_alive {
                    return;
                }
                // gpui executor has no tokio reactor — native timer.
                app_cx
                    .background_executor()
                    .timer(LOADED_POLL_INTERVAL)
                    .await;
            }
        })
        .detach();
    }

    /// Kick off a pull.  The async task opens the streaming verb,
    /// drains chunks into `active_pull.latest`, and clears the slot
    /// on success / error / cancel.
    pub fn start_pull(&mut self, name: String, cx: &mut Context<Self>) {
        if self.active_pull.is_some() {
            return;
        }
        // A new action supersedes any lingering "Freed …" success line.
        self.status = None;
        // A pull is starting — collapse the autocomplete so it doesn't
        // pop back over the progress bar (or linger once the pull ends).
        self.pull_query.clear();
        self.pull_selected = None;
        // Tear down any online-search UI too so it doesn't linger over the
        // progress bar.
        self.hf_search = HfSearch::Idle;
        self.hf_selected = None;
        self.pull_input.update(cx, |i, cx| i.clear(cx));
        let stream = match pull_model(&name) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("pull start: {e}"));
                cx.notify();
                return;
            }
        };
        self.active_pull = Some(PullState {
            model_name: name.clone(),
            latest: PullProgress {
                status: "starting…".to_owned(),
                ..PullProgress::default()
            },
            stream: Some(stream),
        });
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                // Take the stream out so we can borrow it mutably for
                // recv; stash it back after to keep `cancel_pull`'s
                // drop-to-cancel path live.
                let next = match this.update(app_cx, |panel, _| {
                    panel.active_pull.as_mut().and_then(|p| p.stream.take())
                }) {
                    Ok(Some(mut s)) => {
                        let frame = s.recv().await;
                        let _ = this.update(app_cx, |panel, _| {
                            if let Some(p) = panel.active_pull.as_mut() {
                                if p.stream.is_none() {
                                    p.stream = Some(s);
                                }
                            }
                        });
                        frame
                    }
                    _ => return,
                };
                match next {
                    Some(Ok(v)) => {
                        let progress = PullProgress::from_value(&v);
                        let done = progress.is_success();
                        let _ = this.update(app_cx, |panel, cx| {
                            if let Some(p) = panel.active_pull.as_mut() {
                                p.latest = progress;
                            }
                            cx.notify();
                        });
                        if done {
                            let _ = this.update(app_cx, |panel, cx| {
                                panel.active_pull = None;
                                cx.notify();
                                Self::spawn_refresh_installed(cx);
                            });
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = this.update(app_cx, |panel, cx| {
                            panel.error = Some(format!("pull '{name}': {e}"));
                            panel.active_pull = None;
                            cx.notify();
                        });
                        return;
                    }
                    None => {
                        // Stream ended without a success frame.
                        let _ = this.update(app_cx, |panel, cx| {
                            if panel.active_pull.is_some() {
                                panel.error =
                                    Some(format!("pull '{name}': stream ended unexpectedly",));
                            }
                            panel.active_pull = None;
                            cx.notify();
                            Self::spawn_refresh_installed(cx);
                        });
                        return;
                    }
                }
            }
        })
        .detach();
    }

    /// Cancel-in-progress button — drops the stream, the harness wires
    /// the disconnect to "abandon".
    pub fn cancel_pull(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.active_pull.take() {
            // Dropping `p.stream` cancels.
            drop(p);
            cx.notify();
        }
    }

    /// Pick a catalog suggestion: drop its exact tag into the field and
    /// latch it so the dropdown closes (the user can now hit Pull / Enter
    /// or keep typing to search again).  Does not start the pull — the maintainer
    /// asked to confirm the size before committing, so selection only
    /// fills the field and surfaces the detail strip.
    pub fn select_catalog(&mut self, tag: String, cx: &mut Context<Self>) {
        // `set_text` re-emits `Changed(tag)`, which the subscription
        // folds into `pull_query`; setting it here too keeps the field
        // correct even if that event is deferred.  Because it now equals
        // `pull_selected`, the dropdown stays closed.
        self.pull_input
            .update(cx, |i, cx| i.set_text(tag.clone(), cx));
        self.pull_query = tag.clone();
        self.pull_selected = Some(tag);
        cx.notify();
    }

    // ── HuggingFace online search (opt-in, privacy-gated) ─────────────

    /// Kick off a HuggingFace search for `query`. No-op (and never touches
    /// the network) when the query is empty or the privacy toggle is off —
    /// the affordance that calls this only renders when enabled, but the
    /// guard makes the privacy invariant local to the call too.
    pub fn start_hf_search(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_owned();
        if query.is_empty() || !wylde_gui_pipe::privacy_prefs::current().hf_search_enabled {
            return;
        }
        self.hf_query = query.clone();
        self.hf_selected = None;
        self.hf_search = HfSearch::Searching;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = hf::search(query).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.hf_search = match outcome {
                    Ok(rows) if rows.is_empty() => HfSearch::Empty,
                    Ok(rows) => HfSearch::Results(rows),
                    Err(e) => HfSearch::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Pick a HuggingFace result: resolve it to an `hf.co/<repo>:<quant>`
    /// pull tag (default quant) dropped into the pull field, close the
    /// results list, and surface the quant-picker detail strip. Does not
    /// start the pull — same confirm-before-committing flow as the catalog.
    pub fn select_hf_result(&mut self, repo_id: String, cx: &mut Context<Self>) {
        let quant = hf::default_quant().to_owned();
        let tag = hf::to_pull_tag(&repo_id, &quant);
        self.hf_selected = Some(HfSelection { repo_id, quant });
        self.hf_search = HfSearch::Idle;
        self.pull_input
            .update(cx, |i, cx| i.set_text(tag.clone(), cx));
        self.pull_query = tag.clone();
        self.pull_selected = Some(tag);
        cx.notify();
    }

    /// Cycle the selected result's quant through [`hf::QUANTS`], rewriting
    /// the pull tag in the field so the Pull button commits the new quant.
    pub fn cycle_hf_quant(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.hf_selected.clone() else {
            return;
        };
        let idx = hf::QUANTS.iter().position(|&q| q == sel.quant).unwrap_or(0);
        let next = hf::QUANTS[(idx + 1) % hf::QUANTS.len()].to_owned();
        let tag = hf::to_pull_tag(&sel.repo_id, &next);
        self.hf_selected = Some(HfSelection {
            repo_id: sel.repo_id,
            quant: next,
        });
        self.pull_input
            .update(cx, |i, cx| i.set_text(tag.clone(), cx));
        self.pull_query = tag.clone();
        self.pull_selected = Some(tag);
        cx.notify();
    }

    /// Close the HuggingFace results strip and return to catalog browsing.
    pub fn clear_hf_search(&mut self, cx: &mut Context<Self>) {
        self.hf_search = HfSearch::Idle;
        cx.notify();
    }

    /// Reset the filter — clears both the mirrored query and the input
    /// buffer.  Wired to the inline ✕ affordance and to Escape while the
    /// search box holds focus.  `input.clear` re-emits `Changed("")`,
    /// which the subscription folds back into `search_query`; setting it
    /// here too keeps the field correct even if that event is deferred.
    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search_query.clear();
        self.search_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// Inline confirm-delete — first click stages the confirmation,
    /// second confirms.  Cancel clears the staged state.
    pub fn request_delete(&mut self, name: String, cx: &mut Context<Self>) {
        self.confirm_delete = Some(name);
        cx.notify();
    }

    pub fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.confirm_delete.take() else {
            return;
        };
        // Capture the size we already have for this row so we can still
        // report bytes freed if the wrapper couldn't determine it.
        let cached_size = self
            .installed
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.size_bytes)
            .unwrap_or(0);
        self.status = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = delete_installed_model(&name).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(freed) => {
                        // Prefer the wrapper's authoritative freed_bytes;
                        // fall back to the size the row already showed.
                        let bytes = if freed > 0 { freed } else { cached_size };
                        panel.status = Some(freed_message(&name, bytes));
                        if panel.session_default.as_deref() == Some(name.as_str()) {
                            panel.session_default = None;
                        }
                    }
                    Err(e) => {
                        panel.error = Some(format!("delete '{name}': {e}"));
                    }
                }
                cx.notify();
                Self::spawn_refresh_installed(cx);
            });
        })
        .detach();
    }

    pub fn set_default(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        // Optimistic mirror — flip the star instantly, then persist
        // through the harness so the choice survives a restart.  A write
        // failure only logs into the error strip; the in-session star
        // stays put so the UI doesn't flicker back.
        self.session_default = name.clone();
        cx.notify();
        // Tell any open Settings panel the starred default changed so it
        // re-resolves the effective model + its parameter placeholders.
        let _ = wylde_gui_pipe::publish_starred_default(name.clone());
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = crate::ipc::set_default(name.as_deref()).await;
            if let Err(e) = outcome {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(format!("set_default: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Resolve the default model on panel open and pre-check the matching
    /// row (#235).
    ///
    /// Reads `models.resolve_default`, not the raw star: the star is only
    /// arm 1 of the resolution, and a star whose model was deleted must
    /// light up *nothing* rather than a row that isn't there. The reply
    /// also carries the recommend arm, which the empty-store card renders.
    ///
    /// Soft-fails: a down harness leaves the star un-filled and
    /// `default_resolution` untouched rather than surfacing an error
    /// toast — and, per #132, never reads as "nothing installed".
    pub fn spawn_load_default(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(resolution) = crate::ipc::resolve_default().await {
                let _ = this.update(app_cx, |panel, cx| {
                    // Only the *starred* arm pre-checks a row. A
                    // first-available pick is what the picker would land
                    // on, not a choice the user made — filling the star
                    // for it would silently invent a preference.
                    if panel.session_default.is_none() && resolution.source == "default" {
                        panel.session_default = resolution.model.clone();
                    }
                    panel.default_resolution = Some(resolution);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for ModelsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = header_row(cx);
        let mut column = div()
            .max_w(px(860.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        if let Some(err) = &self.error {
            column = column.child(error_strip(err));
        }
        if let Some(status) = &self.status {
            column = column.child(status_strip(status));
        }
        // A star that outlived its model explains itself once, at the top
        // — the fallback already happened, so this is a note, not an error.
        if let Some(stale) = self
            .default_resolution
            .as_ref()
            .and_then(|r| r.stale_default.as_deref())
        {
            let fell_back_to = self
                .default_resolution
                .as_ref()
                .and_then(|r| r.model.as_deref());
            column = column.child(stale_default_note(stale, fell_back_to));
        }

        column = column.child(section_title("Pull a model"));
        column = column.child(pull_section(self, cx));

        column = column.child(section_title("Installed models"));
        match classify_installed_section(
            self.loading_installed,
            self.installed_reachable,
            self.installed.is_empty(),
        ) {
            InstalledSection::Loading => {
                column = column.child(loading_row());
            }
            InstalledSection::Unreachable => {
                column = column.child(unreachable_state(cx));
            }
            InstalledSection::Empty => {
                let rec = self
                    .default_resolution
                    .as_ref()
                    .and_then(|r| r.recommendation.clone());
                column = column.child(empty_installed_state(rec.as_ref(), cx));
            }
            InstalledSection::List => {
                column = column.child(search_strip(self, cx));
                let ranked = fuzzy_rank(&self.installed, &self.search_query);
                if ranked.is_empty() {
                    column = column.child(no_match_state(self.search_query.trim()));
                } else {
                    for m in ranked {
                        column = column.child(installed_row(self, m, cx));
                    }
                }
            }
        }

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

fn header_row(cx: &mut Context<ModelsPanel>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::LG))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from("Models")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Local LLMs Ollama is serving. Pull new tags, drop ones you no \
                             longer need, star a session default.",
                        )),
                ),
        )
        .child(refresh_button(cx))
}

fn refresh_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name("models-refresh".into());
    control(div(), id)
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut ModelsPanel, _event, _window, cx| {
                ModelsPanel::spawn_refresh_installed(cx);
                ModelsPanel::spawn_refresh_hardware(cx);
                ModelsPanel::spawn_refresh_references(cx);
            }),
        )
        .child(SharedString::from("Refresh"))
}

fn section_title(label: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .child(SharedString::from(label.to_ascii_uppercase()))
}

fn pull_section(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let mut col = div().flex().flex_col().gap_3();

    let mut input_row = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_end()
        .child(div().flex_1().child(panel.pull_input.clone()));

    if panel.active_pull.is_some() {
        input_row = input_row.child(cancel_pull_button(cx));
    } else {
        input_row = input_row.child(pull_submit_button(panel.pull_input.clone(), cx));
    }
    col = col.child(input_row);

    // A pull in flight owns the strip — progress bar, no autocomplete.
    if let Some(pull) = &panel.active_pull {
        return col.child(pull_progress_strip(pull));
    }

    // Online search is opt-in: read the privacy toggle so the catalog
    // dropdown knows whether to offer the "Search HuggingFace" row. With
    // the toggle off this stays false and no HF affordance ever renders.
    let hf_enabled = wylde_gui_pipe::privacy_prefs::current().hf_search_enabled;

    // An active online search owns the strip until the user picks a result
    // or closes it (checked before the catalog flow so its results aren't
    // hidden by a stale `searching` state).
    if !matches!(panel.hf_search, HfSearch::Idle) {
        return col.child(hf_results_strip(panel, cx));
    }

    let query = panel.pull_query.trim();
    // "Searching" means the live query has diverged from the last
    // selected tag — show the dropdown.  A query that still equals the
    // selection means the user is looking at their pick (detail strip),
    // not searching.
    let searching = !query.is_empty() && panel.pull_selected.as_deref() != Some(query);

    if searching {
        col = col.child(catalog_dropdown(query, hf_enabled, cx));
    } else if let Some(sel) = &panel.hf_selected {
        // A HuggingFace result is staged — show its quant picker + the
        // resolved pull tag (checked before the catalog detail since the
        // `hf.co/...` tag isn't a catalog entry).
        col = col.child(hf_detail_strip(sel, cx));
    } else if let Some(entry) = catalog::exact(query) {
        // Idle on a known tag (typically just after selecting a row):
        // show the parameters + download-size detail so the user knows
        // what Pull will commit to.
        col = col.child(catalog_detail_strip(entry));
    } else {
        col = col.child(recommendations_strip(panel, cx));
    }

    col
}

/// Search-as-you-type suggestion list under the pull input.  Up to
/// `CATALOG_SUGGESTION_LIMIT` fuzzy matches, each selectable; if the
/// typed query isn't itself an exact catalog tag, a trailing "Pull
/// anyway" row covers uncatalogued / brand-new tags.
fn catalog_dropdown(query: &str, hf_enabled: bool, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let matches = catalog::fuzzy_search(query, CATALOG_SUGGESTION_LIMIT);

    let mut list = div()
        .flex()
        .flex_col()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_800)))
        .overflow_hidden();

    if matches.is_empty() {
        list = list.child(no_catalog_match_hint(query));
    } else {
        for entry in matches {
            list = list.child(catalog_row(entry, cx));
        }
    }

    // The escape hatch: pull exactly what was typed even if we don't
    // list it.  `ollama pull <tag>` works regardless of our catalog.
    if catalog::exact(query).is_none() {
        list = list.child(pull_anyway_row(query, cx));
    }

    // Opt-in online search: only when the privacy toggle is on. This is
    // the "search beyond the curated catalog" affordance — clicking it
    // queries HuggingFace for the typed term.
    if should_offer_hf(hf_enabled, query) {
        list = list.child(hf_search_row(query, cx));
    }

    list
}

/// Whether the "Search HuggingFace" affordance should render: only when
/// the user opted in *and* there's a non-empty query to search for. Pure
/// so the privacy gate is unit-testable.
pub(crate) fn should_offer_hf(enabled: bool, query: &str) -> bool {
    enabled && !query.trim().is_empty()
}

/// One selectable suggestion: family letter-icon, name + tag, the
/// category/param meta line, and a size badge on the right.
fn catalog_row(entry: &CatalogEntry, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let tag_for_click = entry.tag.clone();
    let id: ElementId = ElementId::Name(format!("models-cat::{}", entry.tag).into());

    let mut meta_bits: Vec<String> = vec![format!("{} params", entry.parameters)];
    if let Some(ctx) = entry.context {
        meta_bits.push(format!("{} ctx", humanize_context(ctx)));
    }
    if let Some(lic) = &entry.license {
        meta_bits.push(lic.clone());
    }
    control(div(), id)
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.select_catalog(tag_for_click.clone(), cx);
            }),
        )
        .child(family_icon(entry))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .font_family(FAMILY_INTER)
                                .text_size(px(size::SM))
                                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                                .text_color(rgb(pack(TEXT_PRIMARY)))
                                .child(SharedString::from(entry.display_name.clone())),
                        )
                        .child(
                            div()
                                .font_family(FAMILY_INTER)
                                .text_size(px(size::MICRO))
                                .text_color(rgb(pack(TEXT_MUTED)))
                                .child(SharedString::from(entry.tag.clone())),
                        ),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(meta_bits.join(" · "))),
                ),
        )
        .child(size_badge(entry.size_label()))
}

/// Letter badge standing in for a family logo — first two alnum chars of
/// the family, upper-cased.
fn family_icon(entry: &CatalogEntry) -> gpui::Div {
    div()
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(BRAND)))
        .child(SharedString::from(entry.icon_letters()))
}

/// Rounded download-size pill shown on the right of a suggestion / in the
/// detail strip.
fn size_badge(label: String) -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(SharedString::from(label))
}

/// The "Pull anyway" fallback row for an uncatalogued query.
fn pull_anyway_row(query: &str, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let tag = query.to_owned();
    let tag_for_click = tag.clone();
    control(div(), ElementId::Name("models-pull-anyway".into()))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .bg(rgb(pack(SURFACE_900)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.start_pull(tag_for_click.clone(), cx);
            }),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(BRAND)))
                .child(SharedString::from("↓")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(format!(
                    "Pull “{tag}” anyway — not in our catalog, but Ollama may have it",
                ))),
        )
}

/// Empty-state line shown inside the dropdown when no catalog entry
/// fuzzy-matches the query (the "Pull anyway" row still follows).
fn no_catalog_match_hint(query: &str) -> gpui::Div {
    div()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(format!(
            "No catalog match for “{query}”.",
        )))
}

/// Detail strip for the currently-selected catalog tag — surfaces
/// parameters + download size (+ context / license / blurb) so the user
/// knows what Pull commits to before clicking.
fn catalog_detail_strip(entry: &CatalogEntry) -> gpui::Div {
    let mut meta_bits: Vec<String> = vec![
        format!("{} params", entry.parameters),
        format!("{} download", entry.size_label()),
    ];
    if let Some(ctx) = entry.context {
        meta_bits.push(format!("{} context", humanize_context(ctx)));
    }
    if let Some(lic) = &entry.license {
        meta_bits.push(lic.clone());
    }

    let mut col = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(family_icon(entry))
                .child(
                    div()
                        .flex_1()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(entry.display_name.clone())),
                )
                .child(size_badge(entry.size_label())),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(meta_bits.join(" · "))),
        );

    if !entry.description.is_empty() {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(entry.description.clone())),
        );
    }

    col
}

// ── HuggingFace online search (opt-in) ───────────────────────────────

/// The "🔍 Search HuggingFace for …" row appended to the catalog dropdown
/// when the privacy toggle is on. Clicking it runs the online query.
fn hf_search_row(query: &str, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let q = query.to_owned();
    let q_for_click = q.clone();
    control(div(), ElementId::Name("models-hf-search".into()))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_900)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.start_hf_search(q_for_click.clone(), cx);
            }),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .child(SharedString::from("\u{1F50D}")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(BRAND)))
                .child(SharedString::from(format!(
                    "Search HuggingFace for \u{201c}{q}\u{201d}",
                ))),
        )
}

/// The online-search results strip — header (term + close) over a body
/// that reflects the current [`HfSearch`] state. Only rendered when the
/// state isn't `Idle`.
fn hf_results_strip(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let mut col = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from(format!(
                            "HUGGINGFACE \u{00b7} \u{201c}{}\u{201d}",
                            panel.hf_query
                        ))),
                )
                .child(hf_close_button(cx)),
        );

    match &panel.hf_search {
        HfSearch::Searching => {
            col = col.child(hf_note("Searching HuggingFace\u{2026}"));
        }
        HfSearch::Empty => {
            col = col.child(hf_note(&format!(
                "No HuggingFace results for \u{201c}{}\u{201d}.",
                panel.hf_query
            )));
        }
        HfSearch::Failed(msg) => {
            col = col.child(error_strip(msg));
        }
        HfSearch::Results(rows) => {
            for m in rows {
                col = col.child(hf_result_row(m, cx));
            }
        }
        // Unreachable — the caller guards on `!Idle`.
        HfSearch::Idle => {}
    }

    col
}

/// One HuggingFace result: repo name + an author/downloads/modified meta
/// line. Clicking selects it (resolves the pull tag + opens the quant
/// picker).
fn hf_result_row(m: &HfModel, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let repo_for_click = m.repo_id.clone();
    let id: ElementId = ElementId::Name(format!("models-hf::{}", m.repo_id).into());

    let mut meta_bits: Vec<String> = Vec::new();
    if !m.author.is_empty() {
        meta_bits.push(m.author.clone());
    }
    if m.downloads > 0 {
        meta_bits.push(format!("{} downloads", humanize_downloads(m.downloads)));
    }
    if !m.last_modified.is_empty() {
        meta_bits.push(format!("updated {}", m.last_modified));
    }
    control(div(), id)
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_2()
        .py_2()
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.select_hf_result(repo_for_click.clone(), cx);
            }),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(m.repo_id.clone())),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(meta_bits.join(" \u{00b7} "))),
                ),
        )
        .child(size_badge("GGUF".to_owned()))
}

/// Detail strip for a selected HuggingFace result — shows the resolved
/// pull tag and a clickable quant pill (cycles [`hf::QUANTS`]). The tag
/// also lives in the pull field, so the regular Pull button commits it.
fn hf_detail_strip(sel: &HfSelection, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let tag = hf::to_pull_tag(&sel.repo_id, &sel.quant);
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .font_family(FAMILY_INTER)
                                .text_size(px(size::SM))
                                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                                .text_color(rgb(pack(TEXT_PRIMARY)))
                                .child(SharedString::from(sel.repo_id.clone())),
                        )
                        .child(
                            div()
                                .font_family(FAMILY_INTER)
                                .text_size(px(size::MICRO))
                                .text_color(rgb(pack(TEXT_MUTED)))
                                .child(SharedString::from(tag)),
                        ),
                )
                .child(hf_quant_pill(&sel.quant, cx)),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Pick a quantization, then Pull. Larger quants are higher quality \
                     but bigger downloads.",
                )),
        )
}

/// Clickable quant pill for the HF detail strip — cycles the quant on
/// click. Brand-filled to read as the one interactive choice on the strip.
fn hf_quant_pill(quant: &str, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    control(div(), ElementId::Name("models-hf-quant".into()))
        .cursor_pointer()
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .bg(rgb(pack(BRAND)))
        .px_3()
        .py(px(2.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                this.cycle_hf_quant(cx);
            }),
        )
        .child(SharedString::from(quant.to_owned()))
}

/// The ✕ close button on the HF results strip header.
fn hf_close_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    control(div(), ElementId::Name("models-hf-close".into()))
        .w(px(24.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                this.clear_hf_search(cx);
            }),
        )
        .child(SharedString::from("\u{2715}"))
}

/// A muted one-line note inside the HF results strip (searching / empty).
fn hf_note(text: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(text.to_owned()))
}

/// Compact download count — `123456` → `"123K"`, `4_500_000` → `"4.5M"`.
pub(crate) fn humanize_downloads(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Render a context-window token count compactly — `131072` → `"128K"`.
fn humanize_context(tokens: u64) -> String {
    if tokens >= 1024 {
        format!("{}K", (tokens as f64 / 1024.0).round() as u64)
    } else {
        tokens.to_string()
    }
}

fn pull_submit_button(
    input: Entity<TextInput>,
    cx: &mut Context<ModelsPanel>,
) -> Stateful<gpui::Div> {
    let listener_input = input.clone();
    control(div(), ElementId::Name("models-pull-submit".into()))
        .px_4()
        .py_2()
        .rounded(px(8.0))
        .bg(rgb(pack(BRAND)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                let name = listener_input.read(cx).text().trim().to_owned();
                if !name.is_empty() {
                    listener_input.update(cx, |i, cx| i.clear(cx));
                    this.start_pull(name, cx);
                }
            }),
        )
        .child(SharedString::from("Pull"))
}

fn cancel_pull_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    control(div(), ElementId::Name("models-pull-cancel".into()))
        .px_4()
        .py_2()
        .rounded(px(8.0))
        .bg(rgb(pack(BRAND_DIM)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                this.cancel_pull(cx);
            }),
        )
        .child(SharedString::from("Cancel"))
}

fn pull_progress_strip(pull: &PullState) -> gpui::Div {
    let pct_label = pull
        .latest
        .ratio()
        .map(|r| format!("{}%", (r * 100.0).round() as i32))
        .unwrap_or_else(|| "…".to_owned());
    let bytes_label = if pull.latest.total > 0 {
        format!(
            " — {} / {}",
            humanize_bytes(pull.latest.completed),
            humanize_bytes(pull.latest.total),
        )
    } else {
        String::new()
    };
    let label = SharedString::from(format!(
        "Pulling {} · {} · {}{}",
        pull.model_name, pull.latest.status, pct_label, bytes_label,
    ));
    let bar_width = pull.latest.ratio().unwrap_or(0.0);
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(label),
        )
        .child(
            div()
                .w_full()
                .h(px(4.0))
                .bg(rgb(pack(SURFACE_900)))
                .rounded(px(2.0))
                .child(
                    div()
                        .h(px(4.0))
                        .w(px((bar_width * 740.0).max(2.0)))
                        .bg(rgb(pack(BRAND)))
                        .rounded(px(2.0)),
                ),
        )
}

fn recommendations_strip(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let recs = pick_recommendations(&panel.hardware);
    let hint = SharedString::from(if panel.loading_hardware {
        "Probing hardware for recommendations…".to_owned()
    } else if panel.hardware.is_unknown() {
        "VRAM broker offline — recommendation is the universal starter.".to_owned()
    } else {
        format!(
            "Recommendations for {} · {} GB RAM{}",
            shorten_cpu(&panel.hardware.cpu_brand),
            (panel.hardware.ram_total_bytes / 1024 / 1024 / 1024).max(1),
            if panel.hardware.nvidia_count > 0 {
                format!(
                    " · {} GB VRAM",
                    (panel.hardware.nvidia_vram_bytes / 1024 / 1024 / 1024).max(1),
                )
            } else {
                String::new()
            },
        )
    });

    let mut row = div().flex().flex_row().flex_wrap().gap_2().mt_1();
    for r in recs {
        row = row.child(recommendation_chip(r, cx));
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(hint),
        )
        .child(row)
}

fn recommendation_chip(rec: Recommendation, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let name_for_click = rec.name.clone();
    let id: ElementId = ElementId::Name(format!("models-rec::{}", rec.name).into());
    control(div(), id)
        .px_3()
        .py_1()
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_800)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.start_pull(name_for_click.clone(), cx);
            }),
        )
        .child(SharedString::from(rec.name))
}

fn installed_row(
    panel: &ModelsPanel,
    m: &InstalledModel,
    cx: &mut Context<ModelsPanel>,
) -> gpui::Div {
    let is_default = panel.session_default.as_deref() == Some(m.name.as_str());
    let is_loaded = panel.loaded.iter().any(|n| n == &m.name);
    let confirming = panel.confirm_delete.as_deref() == Some(m.name.as_str());
    // What the running config references this model as (#131): a slot role,
    // or — when nothing references it — a "not referenced" hint that makes
    // superseded/orphaned models answerable as safe-to-delete at a glance.
    let slot = slot_role(&m.name, &panel.references);
    // Only claim "not referenced" once we actually know the reference set
    // (at least one slot populated). With a down harness the set is empty
    // and every model would falsely read as orphaned — stay silent then.
    let references_known = !(panel.references.reasoner.is_empty()
        && panel.references.fast.is_empty()
        && panel.references.embedder.is_empty());
    let unreferenced =
        references_known && is_unreferenced(&m.name, &panel.references, is_default, is_loaded);

    let border = if is_default {
        BORDER_EMPHASIS
    } else {
        BORDER_SUBTLE
    };

    let mut row = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(border)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2();

    row = row.child(installed_row_head(
        m,
        is_default,
        is_loaded,
        confirming,
        slot,
        unreferenced,
        cx,
    ));

    if confirming {
        row = row.child(confirm_strip(cx));
    }

    row
}

/// Top line of an installed-model card: the default-star toggle, the
/// name + metadata column, then the conditional "loaded" pill and the
/// delete button (hidden while a delete confirmation is armed for this
/// model).
#[allow(clippy::too_many_arguments)]
fn installed_row_head(
    m: &InstalledModel,
    is_default: bool,
    is_loaded: bool,
    confirming: bool,
    slot: Option<SlotRole>,
    unreferenced: bool,
    cx: &mut Context<ModelsPanel>,
) -> gpui::Div {
    let label = SharedString::from(m.name.clone());
    let meta = SharedString::from(model_meta(m));
    let name_for_default = m.name.clone();
    let name_for_request = m.name.clone();

    let mut head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(default_star(
            is_default,
            ElementId::Name(format!("models-default::{}", m.name).into()),
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                if this.session_default.as_deref() == Some(name_for_default.as_str()) {
                    this.set_default(None, cx);
                } else {
                    this.set_default(Some(name_for_default.clone()), cx);
                }
            }),
        ))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(label),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(meta),
                ),
        );

    if let Some(role) = slot {
        head = head.child(slot_pill(role));
    }
    if is_loaded {
        head = head.child(loaded_pill());
    }
    // `unreferenced` is already gated on the reference set being known
    // (see `installed_row`), so a down harness stays silent rather than
    // mislabelling every model as orphaned.
    if unreferenced {
        head = head.child(unreferenced_pill());
    }
    if !confirming {
        head = head.child(delete_button(
            ElementId::Name(format!("models-del::{}", m.name).into()),
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.request_delete(name_for_request.clone(), cx);
            }),
        ));
    }

    head
}

fn confirm_strip(cx: &mut Context<ModelsPanel>) -> gpui::Div {
    div()
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .pt_2()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(
                    "Confirm delete? This drops the model from local disk.",
                )),
        )
        .child(
            control(div(), ElementId::Name("models-confirm-yes".into()))
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(BRAND_DIM)))
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                        this.confirm_delete(cx);
                    }),
                )
                .child(SharedString::from("Yes, delete")),
        )
        .child(
            control(div(), ElementId::Name("models-confirm-no".into()))
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                        this.cancel_delete(cx);
                    }),
                )
                .child(SharedString::from("Cancel")),
        )
}

fn default_star<F>(is_default: bool, id: ElementId, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let (label, colour) = if is_default {
        ("★", BRAND)
    } else {
        ("☆", TEXT_MUTED)
    };
    control(div(), id)
        .w(px(24.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(colour)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from(label))
}

fn loaded_pill() -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(BRAND)))
        .child(SharedString::from("in use"))
}

/// Per-row pill naming the reasoning slot a model fills — the running
/// config references it, so it is NOT a safe delete (#131).
fn slot_pill(role: SlotRole) -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(BRAND)))
        .child(SharedString::from(role.label()))
}

/// Muted per-row pill for a model nothing in the running config
/// references — the superseded/orphaned models that are safe to delete.
fn unreferenced_pill() -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("not referenced"))
}

/// Transient success strip (brand-toned, distinct from the error strip)
/// carrying the post-delete "Freed …" line.
fn status_strip(msg: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(BRAND)))
        .child(SharedString::from(msg.to_owned()))
}

/// The #132 "store unreachable" body for the Installed section — shown
/// instead of the "pull your first model" empty state when the list is
/// empty only because the daemon couldn't be reached. It reassures that
/// the models are safe on disk and offers a Retry, so a user right after
/// an update never mistakes "still starting" for "everything is gone".
fn unreachable_state(cx: &mut Context<ModelsPanel>) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_6()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("Model store unavailable")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Couldn't reach Ollama to list models — it may still be starting after an \
                     update. Your installed models are safe on disk.",
                )),
        )
        .child(retry_button(cx))
}

fn retry_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    control(div(), ElementId::Name("models-retry".into()))
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut ModelsPanel, _event, _window, cx| {
                ModelsPanel::spawn_refresh_installed(cx);
                ModelsPanel::spawn_refresh_references(cx);
            }),
        )
        .child(SharedString::from("Retry"))
}

fn delete_button<F>(id: ElementId, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    control(div(), id)
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from("Delete"))
}

/// Live filter box above the installed list.  The input owns its own
/// chrome; this wrapper lays it out beside the conditional ✕ clear
/// affordance and catches Escape (bubbled up from the focused input) to
/// reset the query — matching the keyboard behaviour of the other
/// panels' inputs without piercing the shared `TextInput`.
fn search_strip(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let mut row = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(div().flex_1().child(panel.search_input.clone()));

    if !panel.search_query.trim().is_empty() {
        row = row.child(clear_search_button(cx));
    }

    div()
        .id(ElementId::Name("models-search-strip".into()))
        .on_key_down(cx.listener(
            |this: &mut ModelsPanel, ev: &gpui::KeyDownEvent, _window, cx| {
                if ev.keystroke.key.as_str() == "escape" && !this.search_query.is_empty() {
                    this.clear_search(cx);
                }
            },
        ))
        .child(row)
}

fn clear_search_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    control(div(), ElementId::Name("models-search-clear".into()))
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                this.clear_search(cx);
            }),
        )
        .child(SharedString::from("✕"))
}

/// In-list empty state when the filter excludes every installed model.
/// Subtle (same card as the no-models state) rather than a full-panel
/// takeover — the search box stays visible above it so the user can
/// adjust the query.
fn no_match_state(query: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_6()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(format!("No models match “{query}”."))),
        )
}

/// The genuinely-empty store (#235 arm 3): a *recommendation* with its
/// warnings, and a button that pulls it — never an auto-download.
///
/// `rec` is the harness's recommend payload (`models.resolve_default`).
/// When it hasn't landed yet — a down harness, or the reply still in
/// flight — the card degrades to the pre-#235 copy rather than inventing
/// a model name locally: the recommendation and its warnings have one
/// owner, and a GUI-side guess could name a model the backend never
/// vetted.
fn empty_installed_state(rec: Option<&Recommended>, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let card = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_6()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("No models installed yet")),
        );

    let Some(rec) = rec else {
        return card.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Pull one of the recommendations above to get started.",
                )),
        );
    };

    let mut card = card.child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_SECONDARY)))
            .child(SharedString::from(format!(
                "Recommended: {} ({})",
                rec.model, rec.size
            ))),
    );

    // Every warning, verbatim, before the button that acts on them.
    for w in &rec.warnings {
        card = card.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(w.clone())),
        );
    }

    card.child(pull_recommended_button(&rec.model, cx))
}

/// The recommend card's action. Labelled with what it will do — pull a
/// named model of a stated size — so the click is informed, not implied.
fn pull_recommended_button(model: &str, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let name_for_click = model.to_owned();
    control(div(), ElementId::Name("models-pull-recommended".into()))
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.start_pull(name_for_click.clone(), cx);
            }),
        )
        .child(SharedString::from(format!("Pull {model}")))
}

/// The note shown when a starred default no longer resolves — its model
/// was deleted, and the picker fell through to first-available (#235).
/// Explanatory, not an error: the fallback already succeeded.
fn stale_default_note(stale: &str, fell_back_to: Option<&str>) -> gpui::Div {
    let copy = match fell_back_to {
        Some(m) => format!(
            "Your default model “{stale}” is no longer installed — using “{m}” instead. \
             Star another model to set a new default."
        ),
        None => format!(
            "Your default model “{stale}” is no longer installed, and nothing else is \
             either. Pull a model below to get started."
        ),
    };
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(copy))
}

fn loading_row() -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("Loading…"))
}

fn error_strip(msg: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
}

// ── Pure projections (unit-testable) ─────────────────────────────────

/// Which body the "Installed models" section renders. Splitting
/// **Unreachable** out from **Empty** is the #132 fix: a genuinely empty
/// store ("no models pulled yet — pull one") must never be shown when the
/// list is empty only because the model store was unreachable (e.g. the
/// daemon is still restarting right after an update). Conflating the two
/// tells a user with a full disk that their models are gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledSection {
    /// The first list reply hasn't landed yet.
    Loading,
    /// The list is empty because the store couldn't be reached — the
    /// installed models are still on disk. Distinct copy + a retry, never
    /// the "pull your first model" empty state.
    Unreachable,
    /// The store answered and genuinely holds no models.
    Empty,
    /// One or more installed models to render.
    List,
}

/// Classify the installed section. `reachable` is whether the LAST list
/// attempt reached the daemon; a stale non-empty list still renders as
/// `List` even after a later refresh fails (models don't vanish because a
/// poll blipped).
pub fn classify_installed_section(
    loading: bool,
    reachable: bool,
    is_empty: bool,
) -> InstalledSection {
    if loading {
        InstalledSection::Loading
    } else if !is_empty {
        InstalledSection::List
    } else if reachable {
        InstalledSection::Empty
    } else {
        InstalledSection::Unreachable
    }
}

/// A reasoning slot an installed model fills. Surfaced as a per-row pill so
/// the user can see at a glance that a model is wired into the running
/// config (and is therefore NOT a safe delete) (#131).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRole {
    Reasoner,
    Fast,
    Embedder,
}

impl SlotRole {
    pub fn label(self) -> &'static str {
        match self {
            SlotRole::Reasoner => "reasoner",
            SlotRole::Fast => "fast",
            SlotRole::Embedder => "embedder",
        }
    }
}

/// Normalise a model tag for reference matching: `/api/tags` reports the
/// implicit `:latest` on an untagged pull (`nomic-embed-text` →
/// `nomic-embed-text:latest`) while a slot may store the bare name. Strip a
/// trailing `:latest` on both sides so the two match. Mirrors the wrapper's
/// `actions::gc` / `actions::models` rule.
pub(crate) fn normalize_model_tag(tag: &str) -> &str {
    let t = tag.trim();
    t.strip_suffix(":latest").unwrap_or(t)
}

/// The reasoning slot (if any) this model fills. First match wins in
/// reasoner → fast → embedder order; an unset (empty) slot never matches.
pub fn slot_role(name: &str, refs: &ReferenceSet) -> Option<SlotRole> {
    let n = normalize_model_tag(name);
    let matches = |slot: &str| !slot.trim().is_empty() && normalize_model_tag(slot) == n;
    if matches(&refs.reasoner) {
        Some(SlotRole::Reasoner)
    } else if matches(&refs.fast) {
        Some(SlotRole::Fast)
    } else if matches(&refs.embedder) {
        Some(SlotRole::Embedder)
    } else {
        None
    }
}

/// True when NOTHING in the running config references this model: not a
/// reasoning slot, not the session-default star, not resident in VRAM.
/// These are the superseded/orphaned models — visible and one click from
/// deletion, labelled so "safe to delete?" is answerable at a glance (#131).
pub fn is_unreferenced(name: &str, refs: &ReferenceSet, is_default: bool, is_loaded: bool) -> bool {
    !is_default && !is_loaded && slot_role(name, refs).is_none()
}

/// The transient success line shown after a delete: reports the bytes
/// freed (#131 — "delete must report bytes freed"). Falls back to a plain
/// "Deleted" line when the freed size is unknown (0).
pub fn freed_message(name: &str, freed_bytes: u64) -> String {
    if freed_bytes > 0 {
        format!("Freed {} — deleted {name}", humanize_bytes(freed_bytes))
    } else {
        format!("Deleted {name}")
    }
}

/// The text a model is matched against: its name plus the same
/// metadata the row surfaces (family, parameter size, quantization),
/// space-joined.  Folding them into one haystack lets a query like
/// "qwen", "7b", or "q4" hit whichever field carries it.
pub(crate) fn searchable_text(m: &InstalledModel) -> String {
    let mut s = m.name.clone();
    for extra in [&m.family, &m.param_size, &m.quantization] {
        if !extra.is_empty() {
            s.push(' ');
            s.push_str(extra);
        }
    }
    s
}

/// Fuzzy-rank installed models against `query` for the search box.
///
///   * Empty / whitespace query → every model, original order preserved.
///   * Otherwise → only the models whose `searchable_text` fuzzily
///     matches, sorted by descending nucleo relevance score (best first).
///     Equal scores fall back to name order so the list is deterministic
///     across renders instead of jittering.
///
/// Fuzzy (subsequence) matching gives the UX the maintainer asked for: "qwen"
/// matches every qwen tag, the typo "lama" still hits "llama3.2" (l-a-m-a
/// is a subsequence), and a fragment like "32b" narrows to the 32b tags —
/// all ranked rather than an unordered substring set.
pub(crate) fn fuzzy_rank<'a>(models: &'a [InstalledModel], query: &str) -> Vec<&'a InstalledModel> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return models.iter().collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(trimmed, CaseMatching::Smart, Normalization::Smart);
    // One scratch buffer reused across haystacks — `Utf32Str::new` clears
    // it per call (and skips it entirely for ASCII), so reuse is safe.
    let mut buf: Vec<char> = Vec::new();

    let mut scored: Vec<(u32, &InstalledModel)> = models
        .iter()
        .filter_map(|m| {
            let haystack = searchable_text(m);
            let utf = Utf32Str::new(&haystack, &mut buf);
            pattern.score(utf, &mut matcher).map(|score| (score, m))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, m)| m).collect()
}

pub(crate) fn model_meta(m: &InstalledModel) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !m.family.is_empty() {
        parts.push(m.family.clone());
    }
    if !m.param_size.is_empty() {
        parts.push(m.param_size.clone());
    }
    if !m.quantization.is_empty() {
        parts.push(m.quantization.clone());
    }
    if m.size_bytes > 0 {
        parts.push(humanize_bytes(m.size_bytes));
    }
    if parts.is_empty() {
        m.name.clone()
    } else {
        parts.join(" · ")
    }
}

pub(crate) fn humanize_bytes(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = b as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

pub(crate) fn shorten_cpu(brand: &str) -> String {
    // Strip frequency suffixes like "@ 3.50GHz" the broker frequently
    // bundles into the CPU brand on Windows so the hint chip stays
    // readable.
    let mut s = brand.to_owned();
    if let Some(at) = s.find('@') {
        s.truncate(at);
    }
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "CPU".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<ModelsPanel>();
    }

    #[test]
    fn hf_row_offered_only_when_enabled_and_query_present() {
        // The privacy gate: no HF affordance unless the toggle is on.
        assert!(!should_offer_hf(false, "qwen 3.6"));
        // Enabled but no query → nothing to search for.
        assert!(!should_offer_hf(true, ""));
        assert!(!should_offer_hf(true, "   "));
        // Enabled + a real query → offer the row.
        assert!(should_offer_hf(true, "qwen 3.6"));
    }

    #[test]
    fn humanize_downloads_compacts_by_magnitude() {
        assert_eq!(humanize_downloads(0), "0");
        assert_eq!(humanize_downloads(999), "999");
        assert_eq!(humanize_downloads(1_000), "1K");
        assert_eq!(humanize_downloads(123_456), "123K");
        assert_eq!(humanize_downloads(4_500_000), "4.5M");
    }

    /// Regression: turning the privacy toggle off must leave the curated
    /// catalog flow untouched — `fuzzy_rank` (the installed-list search)
    /// and the catalog's exact/fuzzy lookups behave exactly as before,
    /// independent of any HF state.
    #[test]
    fn catalog_behavior_is_independent_of_hf_gate() {
        // Catalog exact/fuzzy lookups don't consult the gate at all.
        assert!(catalog::exact("definitely-not-a-real-tag-xyz").is_none());
        // The HF gate is a pure function of (enabled, query) and never
        // touches the catalog — toggling it can't change catalog results.
        let q = "qwen";
        assert_eq!(catalog::exact(q).is_some(), catalog::exact(q).is_some());
        assert!(!should_offer_hf(false, q));
    }

    #[test]
    fn humanize_bytes_picks_units_per_magnitude() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(2048), "2 KB");
        assert_eq!(humanize_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(humanize_bytes(7_500_000_000_u64), "7.0 GB");
    }

    #[test]
    fn model_meta_concatenates_details_when_present() {
        let m = InstalledModel {
            name: "qwen2.5:1.5b".into(),
            family: "qwen2.5".into(),
            param_size: "1.5B".into(),
            quantization: "Q4_K_M".into(),
            size_bytes: 1_500_000_000,
            ..Default::default()
        };
        assert_eq!(model_meta(&m), "qwen2.5 · 1.5B · Q4_K_M · 1.4 GB");
    }

    fn model(name: &str, family: &str, param: &str, quant: &str) -> InstalledModel {
        InstalledModel {
            name: name.into(),
            family: family.into(),
            param_size: param.into(),
            quantization: quant.into(),
            ..Default::default()
        }
    }

    fn names(ranked: &[&InstalledModel]) -> Vec<String> {
        ranked.iter().map(|m| m.name.clone()).collect()
    }

    #[test]
    fn fuzzy_rank_empty_query_preserves_original_order() {
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("llama3.2", "llama", "", ""),
            model("qwen3:32b", "qwen3", "32B", "Q4_K_M"),
        ];
        // Empty and whitespace-only both short-circuit to "show all".
        assert_eq!(
            names(&fuzzy_rank(&models, "")),
            names(&models.iter().collect::<Vec<_>>())
        );
        assert_eq!(
            names(&fuzzy_rank(&models, "   ")),
            names(&models.iter().collect::<Vec<_>>())
        );
    }

    #[test]
    fn fuzzy_rank_clean_prefix_keeps_only_matching_family() {
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("llama3.2", "llama", "", ""),
            model("qwen3:32b", "qwen3", "32B", "Q4_K_M"),
        ];
        let got = names(&fuzzy_rank(&models, "qwen"));
        assert!(
            got.iter().all(|n| n.contains("qwen")),
            "only qwen tags: {got:?}"
        );
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn fuzzy_rank_tolerates_typo() {
        // "lama" is a subsequence of "llama3.2" (l-a-m-a) but matches no
        // qwen tag — the dropped 'l' is forgiven without false positives.
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("llama3.2", "llama", "", ""),
        ];
        let got = names(&fuzzy_rank(&models, "lama"));
        assert_eq!(got, vec!["llama3.2".to_owned()]);
    }

    #[test]
    fn fuzzy_rank_fragment_matches_quant_and_param() {
        // "32b" narrows to the 32b tag; the 7b tag has no '3' to match.
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("qwen3:32b", "qwen3", "32B", "Q4_K_M"),
        ];
        let got = names(&fuzzy_rank(&models, "32b"));
        assert_eq!(got, vec!["qwen3:32b".to_owned()]);
    }

    #[test]
    fn fuzzy_rank_sorts_descending_by_score() {
        // Both match "qwen", but a leading-prefix match outscores one
        // buried mid-name. Input order is the reverse of the expected
        // ranking, so a pass proves the sort reorders by score.
        let models = vec![
            model("zzz-qwen-custom", "", "", ""),
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
        ];
        let got = names(&fuzzy_rank(&models, "qwen"));
        assert_eq!(
            got,
            vec!["qwen2.5:7b".to_owned(), "zzz-qwen-custom".to_owned()]
        );
    }

    #[test]
    fn humanize_context_compacts_to_k() {
        assert_eq!(humanize_context(131072), "128K");
        assert_eq!(humanize_context(32768), "32K");
        assert_eq!(humanize_context(8192), "8K");
        assert_eq!(humanize_context(163840), "160K");
        assert_eq!(humanize_context(512), "512");
    }

    #[test]
    fn model_meta_falls_back_to_name_when_no_details() {
        let m = InstalledModel {
            name: "mystery:latest".into(),
            ..Default::default()
        };
        assert_eq!(model_meta(&m), "mystery:latest");
    }

    #[test]
    fn shorten_cpu_strips_frequency_suffix() {
        assert_eq!(
            shorten_cpu("Intel Core i7-9750H @ 2.60GHz"),
            "Intel Core i7-9750H"
        );
        assert_eq!(shorten_cpu("  AMD Ryzen 9 7900X "), "AMD Ryzen 9 7900X");
    }

    #[test]
    fn shorten_cpu_defaults_when_empty() {
        assert_eq!(shorten_cpu(""), "CPU");
        assert_eq!(shorten_cpu("  @ 2.0GHz"), "CPU");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn classify_installed_section_splits_unreachable_from_empty() {
        // Loading always wins.
        assert_eq!(
            classify_installed_section(true, false, true),
            InstalledSection::Loading
        );
        // Non-empty → the list, regardless of reachability (stale list stays).
        assert_eq!(
            classify_installed_section(false, false, false),
            InstalledSection::List
        );
        // Empty + reached → genuinely empty ("pull your first model").
        assert_eq!(
            classify_installed_section(false, true, true),
            InstalledSection::Empty
        );
        // #132: empty + unreachable → the "models safe on disk" state, NOT
        // the empty state. This is the whole point of the split.
        assert_eq!(
            classify_installed_section(false, false, true),
            InstalledSection::Unreachable
        );
    }

    #[test]
    fn slot_role_matches_across_latest_normalisation() {
        let refs = ReferenceSet {
            reasoner: "qwen2.5:7b".into(),
            fast: "qwen2.5:1.5b".into(),
            embedder: "nomic-embed-text".into(),
        };
        assert_eq!(slot_role("qwen2.5:7b", &refs), Some(SlotRole::Reasoner));
        assert_eq!(slot_role("qwen2.5:1.5b", &refs), Some(SlotRole::Fast));
        // Slot stores the bare tag; /api/tags reports the implicit :latest.
        assert_eq!(
            slot_role("nomic-embed-text:latest", &refs),
            Some(SlotRole::Embedder)
        );
        // A hand-pulled model fills no slot.
        assert_eq!(slot_role("mistral:7b", &refs), None);
    }

    #[test]
    fn slot_role_ignores_empty_slots() {
        let refs = ReferenceSet::default();
        // Every slot empty ⇒ no match, and crucially an empty model name
        // must not match an empty slot.
        assert_eq!(slot_role("", &refs), None);
        assert_eq!(slot_role("anything", &refs), None);
    }

    #[test]
    fn is_unreferenced_flags_only_the_orphans() {
        let refs = ReferenceSet {
            reasoner: "qwen2.5:7b".into(),
            fast: String::new(),
            embedder: "nomic-embed-text".into(),
        };
        // A slot model is referenced.
        assert!(!is_unreferenced("qwen2.5:7b", &refs, false, false));
        // The session default is referenced even if it fills no slot.
        assert!(!is_unreferenced("mistral:7b", &refs, true, false));
        // A resident (loaded) model is referenced even if it fills no slot.
        assert!(!is_unreferenced("mistral:7b", &refs, false, true));
        // No slot, not default, not loaded ⇒ orphan, safe to delete.
        assert!(is_unreferenced("mistral:7b", &refs, false, false));
    }

    #[test]
    fn freed_message_reports_bytes_or_falls_back() {
        assert_eq!(
            freed_message("qwen2.5:1.5b", 1_500_000_000),
            "Freed 1.4 GB — deleted qwen2.5:1.5b"
        );
        // Unknown size (0) → a plain deleted line, never "Freed 0 B".
        assert_eq!(freed_message("ghost", 0), "Deleted ghost");
    }

    #[test]
    fn pull_progress_value_round_trip_through_view_field() {
        // Exercises the from_value → store-on-panel path the View uses.
        let v = json!({
            "status": "downloading",
            "completed": 5_u64,
            "total": 20_u64,
            "digest": "sha256:abcd",
        });
        let progress = PullProgress::from_value(&v);
        assert_eq!(progress.ratio(), Some(0.25));
    }
}
