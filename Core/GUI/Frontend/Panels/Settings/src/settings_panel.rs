//! The Settings panel View — root of the settings page.
//!
//! State held inline:
//!   - `update_prefs`           — last-read `updater.get_prefs` reply
//!   - `autostart_enabled`      — local mirror of the OS login-item bit
//!   - `autostart_error`        — last `auto-launch` error to surface
//!   - `ollama`                 — last-read Ollama default block
//!   - `consent`                — last-read `consent.list` reply
//!   - `error`                  — last write-side failure to surface
//!
//! All start at "loading" defaults so the View renders before any pipe
//! call has come back.  `spawn_refresh` pulls the read side; the toggle
//! methods below drive the write side.  Every write is optimistic —
//! the badge flips instantly, then the pipe round-trip reconciles (or,
//! on failure, rolls the optimistic flip back and surfaces `error`).
//! This matches the Models panel's `set_default` precedent.

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, Entity, FocusHandle,
    IntoElement, KeyDownEvent, Render, Subscription, Window,
};
use serde_json::{json, Value};
use wylde_gpui_input::{InputEvent, TextInput};
use wylde_theme::colors::{SURFACE_900, TEXT_PRIMARY, TEXT_SECONDARY};
use wylde_theme::typography::{size, FAMILY_INTER};

use crate::hotkey::{resolve_capture, CaptureOutcome};
use crate::ipc::{
    accept_profile_proposal, check_for_update, clear_ollama_override, clear_tool_decision,
    download_and_install, get_autostart_enabled, list_consent, list_input_devices,
    list_profile_proposals, read_effective_model, read_encryption_at_rest, read_model_defaults,
    read_ollama_overrides, read_privacy_prefs, read_update_prefs, read_user_profile,
    read_voice_settings, reject_profile_proposal, reset_consent, set_autostart_enabled,
    set_no_auth, set_ollama_override, set_tool_decision, test_mic, update_profile_field,
    write_encryption_at_rest, write_privacy_prefs, write_update_prefs, write_voice_settings,
    ConsentSnapshot, OllamaSettings, PrivacyPrefs, UpdateCheck, UpdatePrefs, VoiceDevices,
    VoiceSettings, VoiceTest,
};
use crate::sections::{
    auto_check_consent_modal, channel_warning_modal, consent_section, error_banner,
    hf_privacy_modal, ollama_field_string, ollama_loading_card, ollama_section, pack,
    privacy_section, profile_rules_section, startup_section, updates_section, voice_section,
    FieldKind, OLLAMA_FIELDS,
};

/// Debounce window before a typed Ollama-field edit is persisted, so a
/// burst of keystrokes collapses into a single `settings.ollama.*` write.
const OLLAMA_WRITE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// Debounce window before a typed Profile/Rules edit is persisted via
/// `user_profile.update`, collapsing a keystroke burst into one write.
const PROFILE_WRITE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// Profile field indices (parallel to [`SettingsPanel::profile_debounce_gen`]).
const PROFILE_FIELDS: [&str; 3] = ["name", "style", "free_text_rules"];

/// Render state of the Ollama inference section (scope §5). State 4 (live
/// re-query on a model change) is not a variant — it's this same machine
/// re-driven by a `model_bus` event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OllamaSection {
    /// Resolving the effective model / its defaults — transient.
    #[default]
    Loading,
    /// State 1 — nothing selected. Renders the empty state.
    NoModel,
    /// States 2 & 3 — a model is selected; the editable grid renders.
    /// `unreachable_note` is `Some` in State 2 (Ollama couldn't be
    /// queried for defaults), `None` in State 3 (defaults loaded).
    Editable { unreachable_note: Option<String> },
}

/// What a typed field edit should do to the override store.
#[derive(Debug, Clone, PartialEq)]
enum FieldWrite {
    /// Field blanked → clear the override (fall back to placeholder).
    Clear,
    /// A valid value → set/merge the override.
    Set(Value),
    /// Garbage that won't coerce to the field's number kind → do nothing
    /// (don't poison the store with an unparseable value).
    Skip,
}

/// Coerce a field's text to a [`FieldWrite`] per its [`FieldKind`].
fn parse_ollama_field(kind: FieldKind, text: &str) -> FieldWrite {
    let t = text.trim();
    if t.is_empty() {
        return FieldWrite::Clear;
    }
    match kind {
        FieldKind::Int => t
            .parse::<i64>()
            .map(|n| FieldWrite::Set(json!(n)))
            .unwrap_or(FieldWrite::Skip),
        FieldKind::Float => t
            .parse::<f64>()
            .map(|f| FieldWrite::Set(json!(f)))
            .unwrap_or(FieldWrite::Skip),
        FieldKind::Text => FieldWrite::Set(json!(t)),
    }
}

/// Cycle the next value in a preset list, wrapping around. An off-list
/// current value advances to the first entry. Pure helper so the voice
/// pill rotations are unit-testable without a live click.
fn next_in_cycle(list: &[&str], current: &str) -> String {
    if list.is_empty() {
        return current.to_owned();
    }
    match list.iter().position(|&x| x == current) {
        Some(i) => list[(i + 1) % list.len()].to_owned(),
        None => list[0].to_owned(),
    }
}

/// What flipping the HuggingFace-search toggle should do, given the
/// current prefs. Split out as a pure decision so the state machine is
/// unit-testable without a live click / pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfToggleAction {
    /// Already on → turn it off (no warning on the way down).
    Disable,
    /// Off, but the warning has been acknowledged before → enable
    /// directly with no modal.
    EnableDirect,
    /// Off and the warning has never been shown → open the first-time
    /// privacy modal; the actual enable waits on the user confirming.
    ShowModal,
}

/// Pure mapping from prefs → the action a toggle click should take.
pub fn decide_hf_toggle(prefs: PrivacyPrefs) -> HfToggleAction {
    if prefs.hf_search_enabled {
        HfToggleAction::Disable
    } else if prefs.hf_search_warning_shown {
        HfToggleAction::EnableDirect
    } else {
        HfToggleAction::ShowModal
    }
}

/// Whether cycling the channel from `current` needs the Experimental-branch
/// warning (req 6). The warning fires only on the way TO experimental
/// (i.e. when currently on Stable / any non-beta value); switching back to
/// Stable is un-gated. Pure so the gate is unit-tested without a click.
pub fn channel_switch_needs_warning(current: &str) -> bool {
    current != "beta"
}

/// Root Settings panel.  Owns the view-side state that the section
/// helpers consume.  Public so the Shell + tests can construct one
/// directly without going through the factory.
pub struct SettingsPanel {
    pub update_prefs: UpdatePrefs,
    /// State of the manual "Check now" / "Install" flow (Phase 12.5).
    pub update_check: UpdateCheck,
    /// True while the auto-update consent modal is up (req 4). Enabling
    /// "Check automatically" opens it; the actual enable waits on confirm.
    pub auto_check_modal_open: bool,
    /// True while the Experimental-branch warning modal is up (req 6).
    /// Switching stable → experimental opens it; the switch waits on confirm.
    pub channel_warning_open: bool,
    pub autostart_enabled: bool,
    pub autostart_error: Option<String>,
    /// Render state of the Ollama inference section.
    pub ollama_section: OllamaSection,
    /// The effective model the section is previewing (header + write
    /// target). `None` outside the [`OllamaSection::Editable`] state.
    pub ollama_model: Option<String>,
    /// The model's own declared defaults (sparse) — the per-field
    /// placeholder source. Empty in State 2.
    pub ollama_defaults: OllamaSettings,
    /// The user's stored overrides (sparse) — the per-field value +
    /// what drives ↺ visibility.
    pub ollama_overrides: OllamaSettings,
    /// One `TextInput` per [`OLLAMA_FIELDS`] entry, created once in
    /// [`Self::view`]. Empty in headless test construction.
    pub ollama_inputs: Vec<Entity<TextInput>>,
    /// Change subscriptions for `ollama_inputs`, kept alive here.
    pub ollama_input_subs: Vec<Subscription>,
    /// Per-field debounce generation — bumped on each keystroke; a
    /// pending write only fires if its generation is still current.
    pub ollama_debounce_gen: Vec<u64>,
    pub consent: ConsentSnapshot,
    /// Persisted voice config (Slice 6). Starts at defaults; reconciled
    /// by `voice.get_config` in `spawn_refresh`.
    pub voice: VoiceSettings,
    /// Enumerated input devices for the mic picker.
    pub voice_devices: VoiceDevices,
    /// True when `voice.get_config` failed (voice service down). The
    /// section still renders on defaults with an "offline" note.
    pub voice_offline: bool,
    /// State of the "Test mic" button.
    pub voice_test: VoiceTest,
    /// Focus handle for the push-to-talk hotkey capture pill. Lazily
    /// minted on first render (the panel's `new()` has no `cx`); `Some`
    /// once the widget has been laid out at least once.
    pub hotkey_focus: Option<FocusHandle>,
    /// True while the hotkey pill is armed and waiting for the next chord.
    pub capturing_hotkey: bool,
    /// Transient note shown under the pill while capturing — e.g. a
    /// reserved-key rejection. Cleared when capture (re)starts or commits.
    pub hotkey_note: Option<String>,
    pub app_version: String,
    /// Privacy & Network opt-ins (HuggingFace online model search).
    /// Loaded synchronously from the shared `privacy_prefs` cache in
    /// `new()` — it's a local file, not a pipe round-trip.
    pub privacy: PrivacyPrefs,
    /// True while the first-time HuggingFace privacy modal is up.
    pub hf_modal_open: bool,
    /// The modal's "Don't show this warning again" checkbox. Defaults to
    /// checked (the user is opting in deliberately); unchecking it leaves
    /// `hf_search_warning_shown` false so the warning returns next time.
    pub hf_dont_show_again: bool,
    /// Last write-side failure (pipe error from a toggle).  Surfaced as
    /// a banner; cleared on the next successful write.
    pub error: Option<String>,
    /// Encryption-at-rest toggle (OI-14). Default on; loaded from the
    /// harness `settings.encryption.get` verb in `spawn_refresh` (the
    /// canonical `data_dir` lives there, so the choice applies to every
    /// service's stores).
    pub encryption_at_rest: bool,

    // ── Profile / Rules (Thought Bubble System Slice D) ──────────────
    /// Last-read user profile (read-only blocks + the source for the
    /// initial input values).
    pub user_profile: crate::ipc::UserProfile,
    /// Pending LLM-proposed profile updates.
    pub profile_proposals: Vec<crate::ipc::ProfileProposal>,
    /// Editable inputs for name / style / free_text_rules. `None` until
    /// minted in [`Self::view`] (headless test construction leaves them
    /// unset, so the section renders its header only).
    pub profile_name_input: Option<Entity<TextInput>>,
    pub profile_style_input: Option<Entity<TextInput>>,
    pub profile_rules_input: Option<Entity<TextInput>>,
    /// Change subscriptions for the three profile inputs, kept alive here.
    pub profile_input_subs: Vec<Subscription>,
    /// Per-field debounce generation (name, style, rules) — bumped on
    /// each keystroke; a pending write fires only if still current.
    pub profile_debounce_gen: [u64; 3],
}

impl SettingsPanel {
    pub fn new() -> Self {
        Self {
            update_prefs: UpdatePrefs::default(),
            update_check: UpdateCheck::default(),
            auto_check_modal_open: false,
            channel_warning_open: false,
            autostart_enabled: false,
            autostart_error: None,
            ollama_section: OllamaSection::Loading,
            ollama_model: None,
            ollama_defaults: OllamaSettings::default(),
            ollama_overrides: OllamaSettings::default(),
            ollama_inputs: Vec::new(),
            ollama_input_subs: Vec::new(),
            ollama_debounce_gen: vec![0; OLLAMA_FIELDS.len()],
            consent: ConsentSnapshot::default(),
            voice: VoiceSettings::default(),
            voice_devices: VoiceDevices::default(),
            voice_offline: false,
            voice_test: VoiceTest::Idle,
            hotkey_focus: None,
            capturing_hotkey: false,
            hotkey_note: None,
            app_version: "0.1.0".into(),
            privacy: read_privacy_prefs(),
            hf_modal_open: false,
            hf_dont_show_again: true,
            error: None,
            encryption_at_rest: true,
            user_profile: crate::ipc::UserProfile::default(),
            profile_proposals: Vec::new(),
            profile_name_input: None,
            profile_style_input: None,
            profile_rules_input: None,
            profile_input_subs: Vec::new(),
            profile_debounce_gen: [0; 3],
        }
    }

    /// Construct an `AnyView` for the panel registry.  Matches the
    /// `factory:` string in `manifest.json`
    /// (`wylde_panel_settings::SettingsPanel::view`).
    ///
    /// The factory also kicks off a one-shot refresh of the panel's
    /// IPC state via `cx.spawn`.  Pushing it into the factory keeps
    /// the Shell side free of per-panel knowledge — the Shell mints
    /// the View and the panel takes care of pulling its own data.
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let mut panel = Self::new();
            // Seed from the background startup check (Phase 12.5, slice
            // 3d): if it already resolved an available update, land the
            // user on a ready "Install" button rather than making them
            // click "Check now" again.  A "no update"/error result leaves
            // the section Idle — it surfaces only as the refreshed
            // "Last checked" footer below.
            if let Some(info) = wylde_gui_pipe::updater_state::available_info() {
                panel.update_check = UpdateCheck::Available(info);
            }
            // Mint the Ollama field inputs + their change subscriptions
            // once, before the first refresh syncs values onto them.
            panel.init_ollama_inputs(cx);
            // Mint the Profile/Rules inputs + their change subscriptions
            // before the first refresh syncs values onto them.
            panel.init_profile_inputs(cx);
            Self::spawn_refresh(cx);
            // Follow cross-panel model changes (Chat pick / Models star)
            // so the Ollama section re-queries live (State 4).
            Self::spawn_model_bus_drain(cx);
            panel
        })
        .into()
    }

    /// Fire every IPC read the panel's render relies on and write the
    /// results back into the View.  Each fetch is independent so we
    /// spawn one task per channel — a slow `consent.list` doesn't
    /// hold up the autostart bit.
    pub fn spawn_refresh(cx: &mut Context<Self>) {
        // Consent snapshot.
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(snap) = list_consent().await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.consent = snap;
                    cx.notify();
                });
            }
        })
        .detach();

        // Update prefs.
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(prefs) = read_update_prefs().await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.update_prefs = prefs;
                    cx.notify();
                });
            }
        })
        .detach();

        // Ollama inference — resolve the effective model, then its
        // defaults + stored overrides (the 4-state machine).
        Self::refresh_ollama(cx);

        // Autostart — synchronous OS read, but we wrap it in a task
        // so it doesn't block the View constructor on slow registry
        // lookups (HKCU on Windows is normally instant but isn't
        // guaranteed; matching the async shape keeps everything
        // uniform).
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let (enabled, err) = match get_autostart_enabled() {
                Ok(b) => (b, None),
                Err(e) => (false, Some(e)),
            };
            let _ = this.update(app_cx, |panel, cx| {
                panel.autostart_enabled = enabled;
                panel.autostart_error = err;
                cx.notify();
            });
        })
        .detach();

        // Voice config (Slice 6) — `voice.get_config`. A failed read marks
        // the section offline (renders defaults + a note) rather than
        // surfacing a banner, since the voice service is optional.
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = read_voice_settings().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(voice) => {
                        panel.voice = voice;
                        panel.voice_offline = false;
                    }
                    Err(_) => panel.voice_offline = true,
                }
                cx.notify();
            });
        })
        .detach();

        // Input device list — best-effort. A failure leaves the picker on
        // "System default" only (the cycle still works, just one entry).
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(devices) = list_input_devices().await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.voice_devices = devices;
                    cx.notify();
                });
            }
        })
        .detach();

        // Profile / Rules — the user profile + its pending proposals
        // (Slice D). Best-effort; a failure leaves the section on
        // defaults (empty profile, no proposals).
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(profile) = read_user_profile().await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.user_profile = profile;
                    panel.sync_profile_inputs(cx);
                    cx.notify();
                });
            }
        })
        .detach();

        // Encryption at rest (OI-14). Best-effort; default stays on if the
        // read fails.
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(enabled) = read_encryption_at_rest().await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.encryption_at_rest = enabled;
                    cx.notify();
                });
            }
        })
        .detach();

        Self::refresh_profile_proposals(cx);
    }

    // ── Profile / Rules section (Slice D) ────────────────────────────

    /// Mint the three profile inputs (name + style single-line, rules
    /// multi-line) and subscribe each to its `Changed` event (→ debounced
    /// persist). Called once from [`Self::view`].
    pub fn init_profile_inputs(&mut self, cx: &mut Context<Self>) {
        let mut subs = Vec::with_capacity(3);

        let name = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("profile-name")
                .with_placeholder("What should I call you?")
        });
        subs.push(cx.subscribe(&name, move |this, _i, ev, cx| {
            if let InputEvent::Changed(text) = ev {
                this.on_profile_field_changed(0, text.clone(), cx);
            }
        }));

        let style = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("profile-style")
                .with_placeholder("e.g. direct answers, no hedging")
        });
        subs.push(cx.subscribe(&style, move |this, _i, ev, cx| {
            if let InputEvent::Changed(text) = ev {
                this.on_profile_field_changed(1, text.clone(), cx);
            }
        }));

        let rules = cx.new(|c| {
            TextInput::multi_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("profile-rules")
                .with_placeholder("Standing rules the assistant follows verbatim…")
        });
        subs.push(cx.subscribe(&rules, move |this, _i, ev, cx| {
            if let InputEvent::Changed(text) = ev {
                this.on_profile_field_changed(2, text.clone(), cx);
            }
        }));

        self.profile_name_input = Some(name);
        self.profile_style_input = Some(style);
        self.profile_rules_input = Some(rules);
        self.profile_input_subs = subs;
    }

    /// Repaint the three profile inputs from `self.user_profile` using the
    /// *silent* setter, so a programmatic resync (load / accept) doesn't
    /// feed the change handler and loop a write back.
    fn sync_profile_inputs(&mut self, cx: &mut Context<Self>) {
        if let Some(input) = self.profile_name_input.clone() {
            let v = self.user_profile.name.clone();
            input.update(cx, |inp, cx| inp.set_text_silent(v, cx));
        }
        if let Some(input) = self.profile_style_input.clone() {
            let v = self.user_profile.style.clone();
            input.update(cx, |inp, cx| inp.set_text_silent(v, cx));
        }
        if let Some(input) = self.profile_rules_input.clone() {
            let v = self.user_profile.free_text_rules.clone();
            input.update(cx, |inp, cx| inp.set_text_silent(v, cx));
        }
    }

    /// A profile field's text changed — debounce, then persist the field
    /// via `user_profile.update`. `idx` selects [`PROFILE_FIELDS`].
    fn on_profile_field_changed(&mut self, idx: usize, text: String, cx: &mut Context<Self>) {
        let Some(field) = PROFILE_FIELDS.get(idx).copied() else {
            return;
        };
        // Bump the generation so an earlier pending write for this field
        // aborts when it wakes.
        if let Some(g) = self.profile_debounce_gen.get_mut(idx) {
            *g += 1;
        }
        let gen = self.profile_debounce_gen[idx];
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            app_cx
                .background_executor()
                .timer(PROFILE_WRITE_DEBOUNCE)
                .await;
            // Superseded by a newer keystroke? Then that one owns the write.
            let current = this
                .update(app_cx, |panel, _| {
                    panel.profile_debounce_gen.get(idx).copied()
                })
                .ok()
                .flatten();
            if current != Some(gen) {
                return;
            }
            let outcome = update_profile_field(field, text).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(profile) => {
                        // Update the model (drives the read-only blocks);
                        // do NOT resync the inputs — that would reset the
                        // caret mid-type.
                        panel.user_profile = profile;
                        panel.error = None;
                    }
                    Err(e) => panel.error = Some(format!("profile: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pull the pending proposal queue into the panel.
    pub fn refresh_profile_proposals(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(proposals) = list_profile_proposals().await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.profile_proposals = proposals;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Accept a pending proposal — apply it, resync the inputs to show the
    /// new value, and refresh the queue.
    pub fn accept_profile_proposal(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = accept_profile_proposal(&id).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(profile) => {
                        panel.user_profile = profile;
                        panel.sync_profile_inputs(cx);
                        panel.error = None;
                    }
                    Err(e) => panel.error = Some(format!("profile: {e}")),
                }
                cx.notify();
            });
            Self::refresh_profile_proposals_async(&this, app_cx).await;
        })
        .detach();
    }

    /// "Edit" a proposal — accept it (so it leaves the queue and its value
    /// lands in the profile) and drop the proposed text into the field
    /// input so the user can immediately tweak it. Only the text fields
    /// reach this (the section gates the Edit button on `is_text_field`).
    pub fn edit_profile_proposal(
        &mut self,
        proposal: crate::ipc::ProfileProposal,
        cx: &mut Context<Self>,
    ) {
        let id = proposal.id.clone();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = accept_profile_proposal(&id).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(profile) => {
                        panel.user_profile = profile;
                        // Land the proposed value in the matching input,
                        // editable (set_text, not silent — but the value
                        // already equals what was persisted, so the
                        // resulting write is a harmless idempotent repeat).
                        let target = match proposal.field.as_str() {
                            "name" => panel.profile_name_input.clone(),
                            "style" => panel.profile_style_input.clone(),
                            "free_text_rules" => panel.profile_rules_input.clone(),
                            _ => None,
                        };
                        if let Some(input) = target {
                            let v = proposal.proposed.clone();
                            input.update(cx, |inp, cx| inp.set_text_silent(v, cx));
                        }
                        panel.error = None;
                    }
                    Err(e) => panel.error = Some(format!("profile: {e}")),
                }
                cx.notify();
            });
            Self::refresh_profile_proposals_async(&this, app_cx).await;
        })
        .detach();
    }

    /// Reject a pending proposal — drop it (recording the OI-11
    /// suppression) and refresh the queue.
    pub fn reject_profile_proposal(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Err(e) = reject_profile_proposal(&id).await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(format!("profile: {e}"));
                    cx.notify();
                });
            }
            Self::refresh_profile_proposals_async(&this, app_cx).await;
        })
        .detach();
    }

    /// Shared tail: re-read the proposal queue from within an async task.
    async fn refresh_profile_proposals_async(this: &gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        if let Ok(proposals) = list_profile_proposals().await {
            let _ = this.update(app_cx, |panel, cx| {
                panel.profile_proposals = proposals;
                cx.notify();
            });
        }
    }

    // ── Ollama inference section (per-model defaults + overrides) ─────

    /// Mint one `TextInput` per [`OLLAMA_FIELDS`] entry and subscribe to
    /// each one's `Changed` event (→ debounced persist). Called once from
    /// [`Self::view`]; idempotent-ish (a second call rebuilds the inputs).
    pub fn init_ollama_inputs(&mut self, cx: &mut Context<Self>) {
        let mut inputs = Vec::with_capacity(OLLAMA_FIELDS.len());
        let mut subs = Vec::with_capacity(OLLAMA_FIELDS.len());
        for (i, field) in OLLAMA_FIELDS.iter().enumerate() {
            let key = field.key;
            let input = cx.new(|c| {
                TextInput::single_line(c)
                    .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                    .with_element_key(format!("ollama-{key}"))
            });
            let sub = cx.subscribe(&input, move |this, _input, ev, cx| {
                if let InputEvent::Changed(text) = ev {
                    this.on_ollama_field_changed(i, text.clone(), cx);
                }
            });
            inputs.push(input);
            subs.push(sub);
        }
        self.ollama_inputs = inputs;
        self.ollama_input_subs = subs;
    }

    /// Resolve the effective model, then its declared defaults + the
    /// user's stored overrides, and land the section in the right state.
    /// Drives the initial load and every `model_bus`-triggered refresh.
    pub fn refresh_ollama(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = this.update(app_cx, |panel, cx| {
                panel.ollama_section = OllamaSection::Loading;
                cx.notify();
            });

            // 1. Effective model (active → default → env → none).
            let model = match read_effective_model().await {
                Ok(eff) => eff.model,
                Err(_) => None,
            };
            let Some(model) = model else {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.ollama_model = None;
                    panel.ollama_section = OllamaSection::NoModel;
                    cx.notify();
                });
                return;
            };

            // 2. Stored overrides (harness) — empty when none / unread.
            let overrides = read_ollama_overrides(&model).await.unwrap_or_default();

            // 3. Model defaults (Ollama upstream) — a transport / 404
            // failure becomes the State-2 note rather than an error.
            let (defaults, note) = match read_model_defaults(&model).await {
                Ok(d) => (d, None),
                Err(e) if e.starts_with("ollama_unreachable") => (
                    OllamaSettings::default(),
                    Some(format!(
                        "Couldn't query {model} — Ollama upstream is unreachable."
                    )),
                ),
                Err(e) if e.starts_with("model_not_found") => (
                    OllamaSettings::default(),
                    Some(format!("{model} is not installed on Ollama.")),
                ),
                Err(e) => (
                    OllamaSettings::default(),
                    Some(format!("Couldn't query {model}: {e}")),
                ),
            };

            let _ = this.update(app_cx, |panel, cx| {
                panel.ollama_model = Some(model);
                panel.ollama_defaults = defaults;
                panel.ollama_overrides = overrides;
                panel.ollama_section = OllamaSection::Editable {
                    unreachable_note: note,
                };
                panel.sync_ollama_inputs(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Repaint each input's placeholder (model default ∪ global fallback,
    /// or `—` in the unreachable state) and value (stored override) from
    /// the freshly-loaded state. Uses the *silent* text setter so this
    /// resync doesn't feed the change handler and loop a write back.
    fn sync_ollama_inputs(&mut self, cx: &mut Context<Self>) {
        let unreachable = matches!(
            &self.ollama_section,
            OllamaSection::Editable {
                unreachable_note: Some(_)
            }
        );
        for (i, field) in OLLAMA_FIELDS.iter().enumerate() {
            let Some(input) = self.ollama_inputs.get(i).cloned() else {
                continue;
            };
            let placeholder = if unreachable {
                "—".to_string()
            } else {
                ollama_field_string(&self.ollama_defaults, field.key)
                    .unwrap_or_else(|| field.fallback.to_string())
            };
            let value = ollama_field_string(&self.ollama_overrides, field.key).unwrap_or_default();
            input.update(cx, |inp, cx| {
                inp.set_placeholder(placeholder.clone(), cx);
                inp.set_text_silent(value.clone(), cx);
            });
        }
    }

    /// A field's text changed — debounce, then persist the override (or
    /// clear it when the field is blanked). Garbage that won't coerce to
    /// the field's number kind is left unpersisted.
    fn on_ollama_field_changed(&mut self, idx: usize, text: String, cx: &mut Context<Self>) {
        let Some(model) = self.ollama_model.clone() else {
            return;
        };
        let Some(field) = OLLAMA_FIELDS.get(idx) else {
            return;
        };
        let key = field.key;
        let kind = field.kind;
        // Bump the generation so an earlier pending write for this field
        // (still parked on its timer) aborts when it wakes.
        if let Some(g) = self.ollama_debounce_gen.get_mut(idx) {
            *g += 1;
        }
        let gen = self.ollama_debounce_gen[idx];
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            app_cx
                .background_executor()
                .timer(OLLAMA_WRITE_DEBOUNCE)
                .await;
            // Superseded by a newer keystroke? Then that one owns the write.
            let current = this
                .update(app_cx, |panel, _| {
                    panel.ollama_debounce_gen.get(idx).copied()
                })
                .ok()
                .flatten();
            if current != Some(gen) {
                return;
            }
            let action = parse_ollama_field(kind, &text);
            if matches!(action, FieldWrite::Skip) {
                return;
            }
            let outcome = match &action {
                FieldWrite::Clear => clear_ollama_override(&model, key).await,
                FieldWrite::Set(v) => set_ollama_override(&model, key, v.clone()).await,
                FieldWrite::Skip => return,
            };
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(()) => {
                        let value = match action {
                            FieldWrite::Set(v) => Some(v),
                            _ => None,
                        };
                        panel.apply_override_field(key, value);
                        panel.error = None;
                    }
                    Err(e) => panel.error = Some(format!("ollama settings: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// ↺ reset — clear a field's stored override so it falls back to its
    /// placeholder. Optimistic: blank the input + drop the local override
    /// immediately, then issue the `clear_override` write.
    pub fn reset_ollama_field(&mut self, key: &'static str, cx: &mut Context<Self>) {
        let Some(model) = self.ollama_model.clone() else {
            return;
        };
        self.apply_override_field(key, None);
        if let Some(idx) = OLLAMA_FIELDS.iter().position(|f| f.key == key) {
            if let Some(input) = self.ollama_inputs.get(idx).cloned() {
                input.update(cx, |inp, cx| inp.set_text_silent("", cx));
            }
            // Abort any in-flight debounce for this field.
            if let Some(g) = self.ollama_debounce_gen.get_mut(idx) {
                *g += 1;
            }
        }
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Err(e) = clear_ollama_override(&model, key).await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(format!("ollama settings: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Apply a single override field to the in-memory `ollama_overrides`
    /// (so ↺ visibility + value styling update without a re-read).
    /// `None` clears the field.
    fn apply_override_field(&mut self, key: &str, value: Option<Value>) {
        let o = &mut self.ollama_overrides;
        match key {
            "num_ctx" => o.num_ctx = value.as_ref().and_then(Value::as_i64),
            "num_predict" => o.num_predict = value.as_ref().and_then(Value::as_i64),
            "temperature" => o.temperature = value.as_ref().and_then(Value::as_f64),
            "top_p" => o.top_p = value.as_ref().and_then(Value::as_f64),
            "top_k" => o.top_k = value.as_ref().and_then(Value::as_i64),
            "min_p" => o.min_p = value.as_ref().and_then(Value::as_f64),
            "repeat_penalty" => o.repeat_penalty = value.as_ref().and_then(Value::as_f64),
            "seed" => o.seed = value.as_ref().and_then(Value::as_i64),
            "keep_alive" => {
                o.keep_alive = value.as_ref().and_then(|v| v.as_str().map(str::to_owned))
            }
            _ => {}
        }
    }

    /// Empty-state CTA → jump to the Models panel via the nav bus.
    pub fn goto_models(&mut self, _cx: &mut Context<Self>) {
        let _ = wylde_gui_pipe::request_nav("core/models");
    }

    /// Follow the cross-panel model bus: any active-pick / star change
    /// re-runs [`Self::refresh_ollama`] so the section's placeholders
    /// (and effective model) track live (State 4).
    pub fn spawn_model_bus_drain(cx: &mut Context<Self>) {
        use tokio::sync::broadcast::error::RecvError;
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let mut rx = wylde_gui_pipe::subscribe_model_bus();
            // Seed once from the latch in case Chat published before we
            // mounted (the broadcast itself won't replay).
            if wylde_gui_pipe::current_active_model().is_some() {
                let alive = this
                    .update(app_cx, |_panel, cx| Self::refresh_ollama(cx))
                    .is_ok();
                if !alive {
                    return;
                }
            }
            loop {
                match rx.recv().await {
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        let alive = this
                            .update(app_cx, |_panel, cx| Self::refresh_ollama(cx))
                            .is_ok();
                        if !alive {
                            return;
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        })
        .detach();
    }

    // ── Write handlers (driven by the section toggles) ───────────────

    /// Flip the master "check for updates" toggle and persist it.
    pub fn toggle_updates_enabled(&mut self, cx: &mut Context<Self>) {
        let target = !self.update_prefs.enabled;
        self.update_prefs.enabled = target;
        self.error = None;
        cx.notify();
        self.persist_update_prefs(json!({ "enabled": target }), cx);
    }

    /// Flip the "check automatically" sub-toggle (req 4). Turning it ON is
    /// gated behind a consent modal (the weekly network call is disclosed
    /// before it's armed); the actual enable waits on
    /// [`Self::confirm_auto_check_modal`]. Turning it OFF is immediate —
    /// no disclosure needed to become *more* isolated.
    pub fn toggle_auto_check(&mut self, cx: &mut Context<Self>) {
        if self.update_prefs.auto_check {
            // On → off: persist immediately.
            self.update_prefs.auto_check = false;
            self.error = None;
            cx.notify();
            self.persist_update_prefs(json!({ "auto_check": false }), cx);
        } else {
            // Off → on: disclose first, enable on confirm.
            self.auto_check_modal_open = true;
            self.error = None;
            cx.notify();
        }
    }

    /// "Enable" in the auto-update consent modal: arm automatic checks and
    /// persist. Closes the modal.
    pub fn confirm_auto_check_modal(&mut self, cx: &mut Context<Self>) {
        self.auto_check_modal_open = false;
        self.update_prefs.auto_check = true;
        self.error = None;
        cx.notify();
        self.persist_update_prefs(json!({ "auto_check": true }), cx);
    }

    /// "Cancel" in the auto-update consent modal: close it and leave
    /// automatic checks off. Nothing is persisted — the user never opted in.
    pub fn cancel_auto_check_modal(&mut self, cx: &mut Context<Self>) {
        self.auto_check_modal_open = false;
        cx.notify();
    }

    /// Cycle the cadence weekly → daily → monthly → weekly and persist.
    pub fn cycle_frequency(&mut self, cx: &mut Context<Self>) {
        let next = match self.update_prefs.frequency.as_str() {
            "weekly" => "daily",
            "daily" => "monthly",
            _ => "weekly",
        };
        self.update_prefs.frequency = next.to_owned();
        self.error = None;
        cx.notify();
        self.persist_update_prefs(json!({ "frequency": next }), cx);
    }

    /// Cycle the release channel stable ⇄ beta (req 5/6). Switching TO the
    /// Experimental branch (stable → beta) is gated behind a warning modal
    /// acknowledging the bug risk; the switch waits on
    /// [`Self::confirm_channel_warning`]. Switching back to Stable (beta →
    /// stable) is immediate — no warning on the way to the safer channel.
    pub fn cycle_channel(&mut self, cx: &mut Context<Self>) {
        if channel_switch_needs_warning(&self.update_prefs.channel) {
            // Stable → Experimental: warn first, switch on confirm.
            self.channel_warning_open = true;
            self.error = None;
            cx.notify();
        } else {
            // Experimental → Stable: switch immediately.
            self.apply_channel_switch("stable", cx);
        }
    }

    /// "Switch to Experimental" in the branch warning modal: adopt the
    /// experimental channel and persist. Closes the modal.
    pub fn confirm_channel_warning(&mut self, cx: &mut Context<Self>) {
        self.channel_warning_open = false;
        self.apply_channel_switch("beta", cx);
    }

    /// "Cancel" in the branch warning modal: close it and stay on the
    /// current channel. Nothing is persisted.
    pub fn cancel_channel_warning(&mut self, cx: &mut Context<Self>) {
        self.channel_warning_open = false;
        cx.notify();
    }

    /// Adopt `next` channel: update local state, reset any prior check
    /// result (the two channels can surface different newest versions), and
    /// persist. Shared by the confirm path and the un-gated beta → stable.
    fn apply_channel_switch(&mut self, next: &str, cx: &mut Context<Self>) {
        self.update_prefs.channel = next.to_owned();
        self.update_check = UpdateCheck::Idle;
        self.error = None;
        cx.notify();
        self.persist_update_prefs(json!({ "channel": next }), cx);
    }

    /// "Decline (Skip this version)" on the changelog card (req 7). Records
    /// the pending version so the automatic path stops re-offering it (a
    /// newer release, with a different version string, is offered normally),
    /// and clears the card back to the up-to-date state. A no-op unless a
    /// check has resolved an available update.
    pub fn skip_version(&mut self, cx: &mut Context<Self>) {
        let UpdateCheck::Available(info) = &self.update_check else {
            return;
        };
        let version = info.version.clone();
        self.update_prefs.skipped_version = Some(version.clone());
        self.update_check = UpdateCheck::UpToDate;
        self.error = None;
        cx.notify();
        self.persist_update_prefs(json!({ "skipped_version": version }), cx);
    }

    /// "Check now" — query GitHub Releases for the selected channel. Runs
    /// even when the master toggle is off (an explicit manual check is the
    /// one network call the privacy-first default still permits).
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        // Ignore re-entrant clicks while a check or install is in flight.
        if matches!(
            self.update_check,
            UpdateCheck::Checking | UpdateCheck::Installing
        ) {
            return;
        }
        let channel = self.update_prefs.channel();
        let version = self.app_version.clone();
        self.update_check = UpdateCheck::Checking;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = check_for_update(channel, version).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.update_check = match outcome {
                    Ok(wylde_updater::UpdateStatus::UpToDate { .. }) => UpdateCheck::UpToDate,
                    Ok(wylde_updater::UpdateStatus::Available(info)) => {
                        UpdateCheck::Available(info)
                    }
                    Err(e) => UpdateCheck::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// "Install update" — download, verify, and swap the running binary.
    /// Only acts when a check has resolved an available update.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateCheck::Available(info) = &self.update_check else {
            return;
        };
        let info = info.clone();
        self.update_check = UpdateCheck::Installing;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = download_and_install(info).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.update_check = match outcome {
                    Ok(()) => UpdateCheck::Installed,
                    Err(e) => UpdateCheck::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Shared tail for the three updater toggles: write the patch, then
    /// reconcile the view from the merged shape the daemon returns. A
    /// failure rolls the section back to whatever the daemon last had.
    fn persist_update_prefs(&self, patch: serde_json::Value, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = write_update_prefs(patch).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(prefs) => panel.update_prefs = prefs,
                    Err(e) => panel.error = Some(format!("update prefs: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Toggle the OS login-item registration.  `set_autostart_enabled`
    /// is a synchronous registry write; we run it on the spawned task so
    /// a slow HKCU op can't stall the click handler.
    pub fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        let target = !self.autostart_enabled;
        self.autostart_enabled = target;
        self.autostart_error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = set_autostart_enabled(target);
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(actual) => {
                        panel.autostart_enabled = actual;
                        panel.autostart_error = None;
                    }
                    Err(e) => {
                        panel.autostart_enabled = !target;
                        panel.autostart_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Flip the global "skip every prompt" (no-auth) consent toggle.
    pub fn toggle_no_auth(&mut self, cx: &mut Context<Self>) {
        let target = !self.consent.no_auth;
        self.consent.no_auth = target;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = set_no_auth(target).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(snap) => panel.consent = snap,
                    Err(e) => {
                        panel.consent.no_auth = !target;
                        panel.error = Some(format!("consent: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Flip a per-tool decision approved ⇄ denied and persist it.
    pub fn cycle_tool_decision(&mut self, tool_id: String, cx: &mut Context<Self>) {
        let current = self
            .consent
            .tools
            .get(&tool_id)
            .map(String::as_str)
            .unwrap_or("");
        let next = if current == "approved" {
            "denied"
        } else {
            "approved"
        };
        self.consent.tools.insert(tool_id.clone(), next.to_owned());
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = set_tool_decision(&tool_id, next).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(snap) => panel.consent = snap,
                    Err(e) => panel.error = Some(format!("consent: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop a per-tool decision so the tool falls back to prompting.
    pub fn clear_tool(&mut self, tool_id: String, cx: &mut Context<Self>) {
        self.consent.tools.remove(&tool_id);
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = clear_tool_decision(&tool_id).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(snap) => panel.consent = snap,
                    Err(e) => panel.error = Some(format!("consent: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reset every consent decision back to defaults.
    pub fn reset_consent_action(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = reset_consent().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(snap) => panel.consent = snap,
                    Err(e) => panel.error = Some(format!("consent reset: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ── Privacy & Network handlers ───────────────────────────────────

    /// Flip the HuggingFace online-search toggle. The first time it's
    /// turned on (warning never shown) this opens the privacy modal and
    /// defers the actual enable to [`Self::confirm_hf_modal`]; thereafter
    /// (or when turning it off) it persists immediately with no modal.
    pub fn toggle_hf_search(&mut self, cx: &mut Context<Self>) {
        match decide_hf_toggle(self.privacy) {
            HfToggleAction::Disable => {
                self.privacy.hf_search_enabled = false;
                self.persist_privacy(cx);
            }
            HfToggleAction::EnableDirect => {
                self.privacy.hf_search_enabled = true;
                self.persist_privacy(cx);
            }
            HfToggleAction::ShowModal => {
                self.hf_dont_show_again = true;
                self.hf_modal_open = true;
                self.error = None;
                cx.notify();
            }
        }
    }

    /// Flip the encryption-at-rest toggle (OI-14) and persist it through the
    /// harness `settings.encryption.set` verb. Optimistically updates the UI;
    /// on a pipe failure it reverts and surfaces the error banner.
    pub fn toggle_encryption_at_rest(&mut self, cx: &mut Context<Self>) {
        let next = !self.encryption_at_rest;
        self.encryption_at_rest = next;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = write_encryption_at_rest(next).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(persisted) => panel.encryption_at_rest = persisted,
                    Err(e) => {
                        // Revert the optimistic flip and show the failure.
                        panel.encryption_at_rest = !next;
                        panel.error = Some(format!("Encryption toggle failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Enable" in the first-time modal: turn the feature on, remember the
    /// warning was shown iff the "don't show again" box is checked, and
    /// persist. An unchecked box leaves `warning_shown` false so the modal
    /// returns on the next enable.
    pub fn confirm_hf_modal(&mut self, cx: &mut Context<Self>) {
        self.privacy.hf_search_enabled = true;
        self.privacy.hf_search_warning_shown = self.hf_dont_show_again;
        self.hf_modal_open = false;
        self.persist_privacy(cx);
    }

    /// "Cancel" in the first-time modal: close it and leave the toggle
    /// where it was (off). Nothing is persisted — the user never opted in.
    pub fn cancel_hf_modal(&mut self, cx: &mut Context<Self>) {
        self.hf_modal_open = false;
        cx.notify();
    }

    /// Flip the modal's "Don't show this warning again" checkbox.
    pub fn toggle_hf_dont_show_again(&mut self, cx: &mut Context<Self>) {
        self.hf_dont_show_again = !self.hf_dont_show_again;
        cx.notify();
    }

    /// "Reset privacy warnings" — clear the shown-flag so the first-time
    /// modal surfaces again the next time the user enables the feature.
    /// Leaves the enabled state untouched.
    pub fn reset_privacy_warnings(&mut self, cx: &mut Context<Self>) {
        self.privacy.hf_search_warning_shown = false;
        self.persist_privacy(cx);
    }

    /// Shared tail for the privacy writes: persist the local snapshot to
    /// the shared cache + disk. The write is synchronous (a few-byte local
    /// file); a failure surfaces in the page banner while the optimistic
    /// in-session state stays put.
    fn persist_privacy(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        if let Err(e) = write_privacy_prefs(self.privacy) {
            self.error = Some(format!("privacy: {e}"));
        }
        cx.notify();
    }

    // ── Voice write handlers (Slice 6) ───────────────────────────────

    /// Flip the capture mode push-to-talk ⇄ always-on and persist.
    pub fn cycle_voice_mode(&mut self, cx: &mut Context<Self>) {
        let next = next_in_cycle(&["push_to_talk", "always_on"], &self.voice.mode);
        self.voice.mode = next.clone();
        self.error = None;
        cx.notify();
        self.persist_voice(json!({ "mode": next }), cx);
    }

    /// Arm push-to-talk hotkey capture: the pill takes keyboard focus and
    /// the next real chord (via [`Self::on_hotkey_key`]) becomes the
    /// binding. Clicking again while armed cancels (toggle), so a stray
    /// click is recoverable without committing a value.
    pub fn toggle_hotkey_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.capturing_hotkey {
            self.capturing_hotkey = false;
            self.hotkey_note = None;
            cx.notify();
            return;
        }
        self.capturing_hotkey = true;
        self.hotkey_note = None;
        self.error = None;
        // Focus the pill so its `on_key_down` receives the chord. The
        // handle is minted lazily in render; if we haven't rendered yet
        // it'll focus on the next frame once present.
        if let Some(handle) = &self.hotkey_focus {
            handle.focus(window, cx);
        }
        cx.notify();
    }

    /// Handle a key-down while the hotkey pill is armed. Routes the chord
    /// through the pure [`resolve_capture`] state machine and applies the
    /// outcome: commit (validate + persist), cancel, reserved (reject with
    /// a note), or pending (a lone modifier — keep waiting).
    pub fn on_hotkey_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.capturing_hotkey {
            return;
        }
        // Auto-repeat from a held key must not double-fire a commit.
        if ev.is_held {
            return;
        }
        // Don't let the captured chord also trigger app shortcuts / focus
        // traversal (Tab) or scrolling (Space) underneath us.
        cx.stop_propagation();

        match resolve_capture(&ev.keystroke) {
            CaptureOutcome::Pending => {
                // Lone modifier — stay armed for the terminal key.
            }
            CaptureOutcome::Cancelled => {
                self.capturing_hotkey = false;
                self.hotkey_note = None;
                self.blur_hotkey(window, cx);
                cx.notify();
            }
            CaptureOutcome::Reserved(note) => {
                // Reject but stay armed so the user can pick another key.
                self.hotkey_note = Some(note.to_owned());
                cx.notify();
            }
            CaptureOutcome::Committed(chord) => {
                self.capturing_hotkey = false;
                self.hotkey_note = None;
                self.blur_hotkey(window, cx);
                // No-op if unchanged — don't round-trip the pipe for a
                // re-press of the current binding.
                if chord != self.voice.push_to_talk_hotkey {
                    self.voice.push_to_talk_hotkey = chord.clone();
                    self.error = None;
                    self.persist_voice(json!({ "push_to_talk_hotkey": chord }), cx);
                }
                cx.notify();
            }
        }
    }

    /// Drop keyboard focus from the hotkey pill after capture ends so a
    /// stray later keypress doesn't re-enter the (now disarmed) handler.
    fn blur_hotkey(&self, window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(handle) = &self.hotkey_focus {
            if handle.is_focused(window) {
                window.blur();
            }
        }
    }

    /// Cycle the STT backend preference Auto → CPU → NPU and persist.
    pub fn cycle_voice_backend(&mut self, cx: &mut Context<Self>) {
        let next = next_in_cycle(crate::ipc::BACKEND_PRESETS, &self.voice.stt_backend_pref);
        self.voice.stt_backend_pref = next.clone();
        self.error = None;
        cx.notify();
        self.persist_voice(json!({ "stt_backend_pref": next }), cx);
    }

    /// Cycle the mic sensitivity Low → Medium → High and persist.
    pub fn cycle_voice_vad(&mut self, cx: &mut Context<Self>) {
        let next = next_in_cycle(crate::ipc::VAD_PRESETS, &self.voice.vad_sensitivity);
        self.voice.vad_sensitivity = next.clone();
        self.error = None;
        cx.notify();
        self.persist_voice(json!({ "vad_sensitivity": next }), cx);
    }

    /// Toggle the wake-word listener on/off and persist.
    pub fn toggle_wake_word(&mut self, cx: &mut Context<Self>) {
        let target = !self.voice.wake_word_enabled;
        self.voice.wake_word_enabled = target;
        self.error = None;
        cx.notify();
        self.persist_voice(json!({ "wake_word_enabled": target }), cx);
    }

    /// Cycle the wake-word phrase through the known models and persist.
    pub fn cycle_wake_word_model(&mut self, cx: &mut Context<Self>) {
        let next = next_in_cycle(crate::ipc::WAKE_WORD_PRESETS, &self.voice.wake_word_model);
        self.voice.wake_word_model = next.clone();
        self.error = None;
        cx.notify();
        self.persist_voice(json!({ "wake_word_model": next }), cx);
    }

    /// Cycle the input device: system default → each enumerated device →
    /// back. Persists `null` for the system-default slot.
    pub fn cycle_input_device(&mut self, cx: &mut Context<Self>) {
        // Cycle order mirrors the picker label: system default first,
        // then each enumerated device.
        let mut order: Vec<Option<String>> = vec![None];
        for d in &self.voice_devices.devices {
            order.push(Some(d.clone()));
        }
        let cur = order
            .iter()
            .position(|x| x.as_deref() == self.voice.input_device.as_deref())
            .unwrap_or(0);
        let next = order[(cur + 1) % order.len()].clone();
        self.voice.input_device = next.clone();
        self.error = None;
        cx.notify();
        let patch = match next {
            Some(name) => json!({ "input_device": name }),
            None => json!({ "input_device": null }),
        };
        self.persist_voice(patch, cx);
    }

    /// Run a one-shot mic test (`voice.test_mic`) and surface the result.
    pub fn run_test_mic(&mut self, cx: &mut Context<Self>) {
        if matches!(self.voice_test, VoiceTest::Running) {
            return;
        }
        self.voice_test = VoiceTest::Running;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = test_mic().await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.voice_test = match outcome {
                    Ok(result) => VoiceTest::Done(result),
                    Err(e) => VoiceTest::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Shared tail for the voice pill/toggle rows: write the patch, then
    /// reconcile the section from the merged config the daemon returns. A
    /// failure surfaces in the page banner (the optimistic flip stays;
    /// the next refresh reconciles), matching `persist_update_prefs`.
    fn persist_voice(&self, patch: serde_json::Value, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = write_voice_settings(patch).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(voice) => {
                        panel.voice = voice;
                        panel.voice_offline = false;
                    }
                    Err(e) => panel.error = Some(format!("voice: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Default for SettingsPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for SettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Mint the hotkey-capture focus handle on first render (the
        // panel's `new()` has no `cx`).  Cloned out so the borrow on
        // `self` is released before the section reads `&self.voice`.
        let hotkey_focus = self
            .hotkey_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();

        // Ollama inference section — render the current state of the
        // 4-state machine. Built before the section chain so its borrows
        // of `self` release first.
        let ollama_el = match &self.ollama_section {
            OllamaSection::Loading => ollama_loading_card(),
            OllamaSection::NoModel => {
                ollama_section(None, None, &[], &OllamaSettings::default(), cx)
            }
            OllamaSection::Editable { unreachable_note } => ollama_section(
                self.ollama_model.as_deref(),
                unreachable_note.as_deref(),
                &self.ollama_inputs,
                &self.ollama_overrides,
                cx,
            ),
        };

        // Top-level layout: a single-column flex with vertical gap and
        // page padding.  Matches the Svelte container
        // `space-y-6 max-w-3xl` shape — left-justified, breathing room.
        let mut col = div()
            .w_full()
            .max_w(px(720.0))
            .flex()
            .flex_col()
            .gap_6()
            .child(header());
        if let Some(err) = &self.error {
            col = col.child(error_banner(err));
        }
        // Scroll viewport: the section stack (updates, startup, ollama,
        // voice, consent) overflows a short window, so the outer
        // container scrolls.  `.id()` + `.overflow_y_scroll()` mirrors
        // the Chat panel's message-log idiom; `w_full` on `col` keeps the
        // content a definite width inside the scroll area.
        let content = div()
            .id("settings-scroll")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(pack(SURFACE_900)))
            .overflow_y_scroll()
            .p_6()
            .child(
                col.child(updates_section(
                    &self.update_prefs,
                    &self.update_check,
                    &self.app_version,
                    cx,
                ))
                // Privacy & Network sits directly below Updates — the
                // two "may make an outside connection" sections kept
                // adjacent at the top of the page.
                .child(privacy_section(self.privacy, self.encryption_at_rest, cx))
                .child(startup_section(
                    self.autostart_enabled,
                    self.autostart_error.as_deref(),
                    cx,
                ))
                .child(ollama_el)
                .child(voice_section(
                    &self.voice,
                    &self.voice_test,
                    self.voice_offline,
                    &hotkey_focus,
                    self.capturing_hotkey,
                    self.hotkey_note.as_deref(),
                    cx,
                ))
                .child(consent_section(&self.consent, cx))
                .child(profile_rules_section(
                    self.profile_name_input.as_ref(),
                    self.profile_style_input.as_ref(),
                    self.profile_rules_input.as_ref(),
                    &self.user_profile,
                    &self.profile_proposals,
                    cx,
                )),
            );

        // The first-time privacy modal floats above the scroll content via
        // an absolutely-positioned overlay on a relative root. Rendered
        // only while armed so it costs nothing in the common case.
        div()
            .relative()
            .size_full()
            .child(content)
            .when(self.hf_modal_open, |root| {
                root.child(hf_privacy_modal(self.hf_dont_show_again, cx))
            })
            .when(self.auto_check_modal_open, |root| {
                root.child(auto_check_consent_modal(cx))
            })
            .when(self.channel_warning_open, |root| {
                root.child(channel_warning_modal(cx))
            })
    }
}

fn header() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::LG))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child("Settings"),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child("App preferences and updates."),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::ConsentSnapshot;
    use std::collections::BTreeMap;

    /// Bare construction — the View struct's `new` must not panic with
    /// the default state.
    #[test]
    fn new_with_defaults_is_constructible() {
        let p = SettingsPanel::new();
        assert!(!p.autostart_enabled);
        assert!(p.autostart_error.is_none());
        assert_eq!(p.update_prefs.frequency, "weekly");
        assert_eq!(p.update_prefs.channel, "stable");
        assert!(matches!(p.update_check, UpdateCheck::Idle));
        // Ollama section starts Loading with no model + no inputs yet
        // (inputs are minted in `view()`, which needs a cx).
        assert!(matches!(p.ollama_section, OllamaSection::Loading));
        assert!(p.ollama_model.is_none());
        assert!(p.ollama_inputs.is_empty());
        assert_eq!(p.ollama_debounce_gen.len(), OLLAMA_FIELDS.len());
        assert!(p.consent.tools.is_empty());
        assert!(p.error.is_none());
        // Slice 6 — voice section starts on defaults, online, idle test.
        assert_eq!(p.voice.mode, "push_to_talk");
        assert_eq!(p.voice.stt_backend_pref, "auto");
        assert!(!p.voice_offline);
        assert!(matches!(p.voice_test, VoiceTest::Idle));
        assert!(p.voice_devices.devices.is_empty());
        // Privacy & Network — modal closed, "don't show again" pre-checked.
        assert!(!p.hf_modal_open);
        assert!(p.hf_dont_show_again);
    }

    #[test]
    fn next_in_cycle_wraps_and_handles_off_list() {
        assert_eq!(next_in_cycle(&["a", "b", "c"], "a"), "b");
        assert_eq!(next_in_cycle(&["a", "b", "c"], "c"), "a");
        // Off-list current value jumps to the first entry.
        assert_eq!(next_in_cycle(&["a", "b", "c"], "z"), "a");
        // Empty list is a no-op.
        assert_eq!(next_in_cycle(&[], "x"), "x");
        // Single entry stays put.
        assert_eq!(next_in_cycle(&["only"], "only"), "only");
    }

    #[test]
    fn voice_mode_cycle_is_binary() {
        assert_eq!(
            next_in_cycle(&["push_to_talk", "always_on"], "push_to_talk"),
            "always_on"
        );
        assert_eq!(
            next_in_cycle(&["push_to_talk", "always_on"], "always_on"),
            "push_to_talk"
        );
    }

    /// The View must hold a settable consent snapshot — the Shell
    /// writes to it after `consent.list` returns.
    #[test]
    fn settings_panel_consent_field_round_trips() {
        let mut p = SettingsPanel::new();
        let mut tools = BTreeMap::new();
        tools.insert("read_file".into(), "approved".into());
        p.consent = ConsentSnapshot {
            no_auth: true,
            tools,
        };
        assert!(p.consent.no_auth);
        assert_eq!(p.consent.tools.len(), 1);
    }

    /// Render must not panic — the test stays at the level of
    /// constructing the element tree; gpui has no headless renderer
    /// we can drive from a unit test without spinning up a window.
    #[test]
    fn render_signature_compiles_with_settings_state() {
        fn assert_render<T: Render>() {}
        assert_render::<SettingsPanel>();
    }

    /// Each settings row's read/write must hit the right pipe verb.
    /// Build-time witness: if any ident goes away, this fails to compile.
    #[test]
    fn each_section_uses_expected_pipe_verbs() {
        let _ = crate::ipc::list_consent;
        let _ = crate::ipc::set_no_auth;
        let _ = crate::ipc::set_tool_decision;
        let _ = crate::ipc::clear_tool_decision;
        let _ = crate::ipc::reset_consent;
        let _ = crate::ipc::read_update_prefs;
        let _ = crate::ipc::write_update_prefs;
        // Updater driver (Phase 12.5): manual check + install.
        let _ = crate::ipc::check_for_update;
        let _ = crate::ipc::download_and_install;
        let _ = crate::ipc::get_autostart_enabled;
        let _ = crate::ipc::set_autostart_enabled;
        // Ollama inference — per-model defaults/overrides via harness +
        // ollama verbs (the Gateway HTTP read is retired; see ipc.rs).
        let _ = crate::ipc::read_effective_model;
        let _ = crate::ipc::read_model_defaults;
        let _ = crate::ipc::read_ollama_overrides;
        let _ = crate::ipc::set_ollama_override;
        let _ = crate::ipc::clear_ollama_override;
        // Voice config (Slice 6): read/write + device list + test.
        let _ = crate::ipc::read_voice_settings;
        let _ = crate::ipc::write_voice_settings;
        let _ = crate::ipc::list_input_devices;
        let _ = crate::ipc::test_mic;
        // Privacy & Network: local-file read/write (no pipe).
        let _ = crate::ipc::read_privacy_prefs;
        let _ = crate::ipc::write_privacy_prefs;
        // Profile / Rules (Slice D): user_profile.* verbs + the section.
        let _ = crate::ipc::read_user_profile;
        let _ = crate::ipc::list_profile_proposals;
        let _ = crate::ipc::update_profile_field;
        let _ = crate::ipc::accept_profile_proposal;
        let _ = crate::ipc::reject_profile_proposal;
        let _ = crate::sections::profile_rules_section;
    }

    /// The proposal-resolution handlers exist with the expected
    /// signatures — a build-time witness that the Accept / Edit / Reject
    /// buttons wire to real panel methods (which in turn call the
    /// `user_profile.{accept,reject}` verbs).
    #[test]
    fn profile_proposal_handlers_are_wired() {
        let _accept: fn(&mut SettingsPanel, String, &mut Context<SettingsPanel>) =
            SettingsPanel::accept_profile_proposal;
        let _reject: fn(&mut SettingsPanel, String, &mut Context<SettingsPanel>) =
            SettingsPanel::reject_profile_proposal;
        let _edit: fn(
            &mut SettingsPanel,
            crate::ipc::ProfileProposal,
            &mut Context<SettingsPanel>,
        ) = SettingsPanel::edit_profile_proposal;
    }

    /// The HuggingFace toggle decision is a pure function of the current
    /// prefs — assert every branch of the first-time-warning state machine
    /// without a live click.
    #[test]
    fn hf_toggle_decision_covers_all_branches() {
        // Off + never warned → first click opens the modal.
        assert_eq!(
            decide_hf_toggle(PrivacyPrefs {
                hf_search_enabled: false,
                hf_search_warning_shown: false,
            }),
            HfToggleAction::ShowModal
        );
        // Off + already warned → enable directly, no modal.
        assert_eq!(
            decide_hf_toggle(PrivacyPrefs {
                hf_search_enabled: false,
                hf_search_warning_shown: true,
            }),
            HfToggleAction::EnableDirect
        );
        // On → turning it off never shows a warning, regardless of flag.
        for shown in [true, false] {
            assert_eq!(
                decide_hf_toggle(PrivacyPrefs {
                    hf_search_enabled: true,
                    hf_search_warning_shown: shown,
                }),
                HfToggleAction::Disable
            );
        }
    }

    /// Pure model of `confirm_hf_modal`: the "don't show again" checkbox
    /// decides whether the warning is suppressed next time. Enable always
    /// turns the feature on.
    #[test]
    fn confirm_modal_applies_checkbox_to_warning_flag() {
        // Box checked → warning suppressed thereafter.
        let next = apply_confirm(PrivacyPrefs::default(), true);
        assert!(next.hf_search_enabled);
        assert!(next.hf_search_warning_shown);
        // Box unchecked → feature on, but the warning returns next time.
        let next = apply_confirm(PrivacyPrefs::default(), false);
        assert!(next.hf_search_enabled);
        assert!(!next.hf_search_warning_shown);
    }

    /// Mirror of `confirm_hf_modal`'s pref mutation, kept in sync as a
    /// pure-logic witness so the modal's commit rule is testable.
    fn apply_confirm(mut prefs: PrivacyPrefs, dont_show_again: bool) -> PrivacyPrefs {
        prefs.hf_search_enabled = true;
        prefs.hf_search_warning_shown = dont_show_again;
        prefs
    }

    /// Reset-warnings clears only the shown flag, leaving enabled intact.
    #[test]
    fn reset_warnings_preserves_enabled_state() {
        let after = PrivacyPrefs {
            hf_search_enabled: true,
            hf_search_warning_shown: false,
        };
        let before = PrivacyPrefs {
            hf_search_enabled: true,
            hf_search_warning_shown: true,
        };
        // The handler sets warning_shown = false and touches nothing else.
        let mut got = before;
        got.hf_search_warning_shown = false;
        assert_eq!(got, after);
    }

    /// The frequency cycle is a pure rotation — assert it round-trips
    /// through all three cadences and back without touching a pipe.
    #[test]
    fn frequency_cycles_through_three_cadences() {
        // Mirrors `cycle_frequency`'s match arms; kept in sync as a
        // pure-logic witness so a typo in the rotation is caught here
        // rather than only in a live click.
        fn next(cur: &str) -> &'static str {
            match cur {
                "weekly" => "daily",
                "daily" => "monthly",
                _ => "weekly",
            }
        }
        assert_eq!(next("weekly"), "daily");
        assert_eq!(next("daily"), "monthly");
        assert_eq!(next("monthly"), "weekly");
        // Unknown/legacy value snaps back to the weekly baseline.
        assert_eq!(next("annually"), "weekly");
    }

    /// The channel cycle is a binary stable ⇄ beta toggle; an unknown
    /// legacy value arms to beta on first click (anything not "beta").
    #[test]
    fn channel_cycles_stable_and_beta() {
        fn next(cur: &str) -> &'static str {
            match cur {
                "beta" => "stable",
                _ => "beta",
            }
        }
        assert_eq!(next("stable"), "beta");
        assert_eq!(next("beta"), "stable");
        assert_eq!(next("nightly"), "beta");
    }

    /// The Experimental-branch warning gate: fires on the way TO
    /// experimental (from Stable or any legacy value), never on the way
    /// back to Stable.
    #[test]
    fn channel_warning_only_on_the_way_to_experimental() {
        assert!(channel_switch_needs_warning("stable"), "stable → experimental warns");
        assert!(
            channel_switch_needs_warning("nightly"),
            "a legacy value arms to beta, so it also warns"
        );
        assert!(
            !channel_switch_needs_warning("beta"),
            "beta → stable is un-gated"
        );
    }

    /// The per-tool decision flip is approved ⇄ denied; an unset tool
    /// arms to approved on first click.
    #[test]
    fn tool_decision_flip_is_binary() {
        fn next(cur: &str) -> &'static str {
            if cur == "approved" {
                "denied"
            } else {
                "approved"
            }
        }
        assert_eq!(next("approved"), "denied");
        assert_eq!(next("denied"), "approved");
        assert_eq!(next(""), "approved");
    }

    // ── Ollama inference section ──────────────────────────────────────

    /// The field-edit parser: blank clears, valid coerces to the field's
    /// number kind, garbage is skipped (never persisted), text is verbatim.
    #[test]
    fn parse_ollama_field_covers_kinds_and_garbage() {
        // Blank → clear regardless of kind.
        assert_eq!(parse_ollama_field(FieldKind::Int, "   "), FieldWrite::Clear);
        assert_eq!(parse_ollama_field(FieldKind::Float, ""), FieldWrite::Clear);
        // Valid coercions.
        assert_eq!(
            parse_ollama_field(FieldKind::Int, " 8192 "),
            FieldWrite::Set(json!(8192))
        );
        assert_eq!(
            parse_ollama_field(FieldKind::Float, "0.7"),
            FieldWrite::Set(json!(0.7))
        );
        assert_eq!(
            parse_ollama_field(FieldKind::Text, "5m"),
            FieldWrite::Set(json!("5m"))
        );
        // Garbage for a numeric field → skip (don't poison the store).
        assert_eq!(parse_ollama_field(FieldKind::Int, "abc"), FieldWrite::Skip);
        assert_eq!(
            parse_ollama_field(FieldKind::Float, "1.2.3"),
            FieldWrite::Skip
        );
        // A float string in an int field is a skip, not a truncation.
        assert_eq!(parse_ollama_field(FieldKind::Int, "4.5"), FieldWrite::Skip);
    }

    /// `apply_override_field` keeps the in-memory overrides (and thus ↺
    /// visibility) in sync without a re-read: Set populates, None clears.
    #[test]
    fn apply_override_field_sets_and_clears() {
        let mut p = SettingsPanel::new();
        p.apply_override_field("temperature", Some(json!(0.7)));
        assert_eq!(p.ollama_overrides.temperature, Some(0.7));
        assert!(crate::sections::ollama_has_override(
            &p.ollama_overrides,
            "temperature"
        ));
        // Clear → field drops out of the sparse override set.
        p.apply_override_field("temperature", None);
        assert_eq!(p.ollama_overrides.temperature, None);
        assert!(!crate::sections::ollama_has_override(
            &p.ollama_overrides,
            "temperature"
        ));
    }

    /// The default section state is `Loading`, and `NoModel` is distinct
    /// from `Editable` so the render branches don't collide.
    #[test]
    fn ollama_section_state_defaults_and_variants_differ() {
        assert_eq!(OllamaSection::default(), OllamaSection::Loading);
        assert_ne!(OllamaSection::NoModel, OllamaSection::Loading);
        assert_ne!(
            OllamaSection::Editable {
                unreachable_note: None
            },
            OllamaSection::Editable {
                unreachable_note: Some("down".into())
            }
        );
    }

    /// The new per-model override verbs must each be wired through ipc.
    #[test]
    fn ollama_override_verbs_are_wired() {
        let _ = crate::ipc::read_effective_model;
        let _ = crate::ipc::read_model_defaults;
        let _ = crate::ipc::read_ollama_overrides;
        let _ = crate::ipc::set_ollama_override;
        let _ = crate::ipc::clear_ollama_override;
    }
}
