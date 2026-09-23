//! Per-panel IPC helpers — the §8 "service's own ipc.rs" pattern.
//!
//! Wraps the bare pipe verbs the Settings panel cares about into
//! typed read/write functions so the View body stays focused on
//! layout.  The verbs themselves live in the harness / lifecycle
//! daemons; this file is purely a thin adapter.
//!
//! All functions are async because the wire-side pipe is async.  The
//! View uses `cx.spawn(...)` to drive them — the same pattern slice 1's
//! shutdown handler set up.

use serde_json::{json, Value};

/// Backend-side shape returned by `consent.list` — mirrors the
/// `consent.set` reply (`{no_auth, tools}` map).  We round-trip the
/// JSON shape so a backend change that adds fields shows up as
/// `extras` rather than an error.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConsentSnapshot {
    pub no_auth: bool,
    /// Per-tool decisions: `tool_id` → `"approved"` / `"denied"`.
    pub tools: std::collections::BTreeMap<String, String>,
}

impl ConsentSnapshot {
    pub fn from_value(v: &Value) -> Self {
        let no_auth = v.get("no_auth").and_then(|x| x.as_bool()).unwrap_or(false);
        let mut tools = std::collections::BTreeMap::new();
        if let Some(obj) = v.get("tools").and_then(|x| x.as_object()) {
            for (k, val) in obj {
                if let Some(s) = val.as_str() {
                    tools.insert(k.clone(), s.to_owned());
                }
            }
        }
        Self { no_auth, tools }
    }
}

/// Read the persisted consent shape via the harness pipe (`consent.list`).
pub async fn list_consent() -> Result<ConsentSnapshot, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({ "action": "consent.list", "payload": {} })),
    )
    .await?;
    Ok(ConsentSnapshot::from_value(&v))
}

/// Toggle the global "no auth" flag (`consent.set_no_auth`).  Returns
/// the snapshot the backend reports after the write.
pub async fn set_no_auth(enabled: bool) -> Result<ConsentSnapshot, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({
            "action": "consent.set_no_auth",
            "payload": { "enabled": enabled },
        })),
    )
    .await?;
    Ok(ConsentSnapshot::from_value(&v))
}

/// Set a per-tool decision (`consent.set`).
pub async fn set_tool_decision(tool_id: &str, decision: &str) -> Result<ConsentSnapshot, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({
            "action": "consent.set",
            "payload": { "tool_id": tool_id, "decision": decision },
        })),
    )
    .await?;
    Ok(ConsentSnapshot::from_value(&v))
}

/// Drop a per-tool decision so the tool falls back to "pending"
/// (`consent.clear`).
pub async fn clear_tool_decision(tool_id: &str) -> Result<ConsentSnapshot, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({
            "action": "consent.clear",
            "payload": { "tool_id": tool_id },
        })),
    )
    .await?;
    Ok(ConsentSnapshot::from_value(&v))
}

/// Reset every consent decision back to defaults (`consent.reset`).
pub async fn reset_consent() -> Result<ConsentSnapshot, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({ "action": "consent.reset", "payload": {} })),
    )
    .await?;
    Ok(ConsentSnapshot::from_value(&v))
}

// ── Updater prefs (Phase 12.5) ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct UpdatePrefs {
    pub enabled: bool,
    pub auto_check: bool,
    pub frequency: String,
    /// Release channel — `"stable"` or `"beta"` (Phase 12.5). Mirrors the
    /// daemon's validated `channel` key; resolved to
    /// [`wylde_updater::Channel`] at the check call site.
    pub channel: String,
    pub last_checked: Option<u64>,
    /// The version the user chose to skip via "Decline" on the changelog
    /// card. `None` until a skip is recorded (or once a newer version
    /// supersedes it). Mirrors the daemon's `skipped_version` key.
    pub skipped_version: Option<String>,
}

impl Default for UpdatePrefs {
    /// Default mirrors what `from_value(&{})` produces — `weekly`
    /// frequency, `stable` channel, everything else off.  Same baseline
    /// the daemon assumes when prefs.json is missing.
    fn default() -> Self {
        Self {
            enabled: false,
            auto_check: false,
            frequency: "weekly".into(),
            channel: "stable".into(),
            last_checked: None,
            skipped_version: None,
        }
    }
}

impl UpdatePrefs {
    pub fn from_value(v: &Value) -> Self {
        Self {
            enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            auto_check: v
                .get("auto_check")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            frequency: v
                .get("frequency")
                .and_then(|x| x.as_str())
                .unwrap_or("weekly")
                .to_owned(),
            channel: v
                .get("channel")
                .and_then(|x| x.as_str())
                .unwrap_or("stable")
                .to_owned(),
            last_checked: v.get("last_checked").and_then(|x| x.as_u64()),
            skipped_version: v
                .get("skipped_version")
                .and_then(|x| x.as_str())
                .map(str::to_owned),
        }
    }

    /// Resolve the persisted channel string to the updater's enum.
    /// Unknown/legacy values fall back to `Stable` (never silently opt a
    /// user into pre-releases).
    pub fn channel(&self) -> wylde_updater::Channel {
        wylde_updater::Channel::from_str_lossy(&self.channel)
    }
}

/// View-side state of the manual update flow (the "Check now" / "Install"
/// buttons). The panel holds one of these; the section renderer turns it
/// into a status line + the right buttons.
#[derive(Debug, Clone, Default)]
pub enum UpdateCheck {
    /// No check run yet this session.
    #[default]
    Idle,
    /// A check is in flight.
    Checking,
    /// Checked: the running build is current.
    UpToDate,
    /// Checked: a newer release is available and resolved.
    Available(wylde_updater::UpdateInfo),
    /// Download + verify + install in flight.
    Installing,
    /// Installed successfully — the new binary applies on next launch.
    Installed,
    /// The last check or install failed; carries the message to surface.
    Failed(String),
}

/// Query GitHub Releases for an update on `channel`, off the gpui executor.
///
/// The updater is a blocking crate; we hop it onto the shared tokio
/// runtime's blocking pool via the Pipe crate's bridge so a `cx.spawn`
/// task (which has no tokio reactor) can await it without panicking.
pub async fn check_for_update(
    channel: wylde_updater::Channel,
    current_version: String,
) -> Result<wylde_updater::UpdateStatus, String> {
    wylde_gui_pipe::bridged_spawn_blocking(move || {
        wylde_updater::check_for_update(channel, &current_version).map_err(|e| e.to_string())
    })
    .await
}

/// Download, verify, and install a resolved update, off the gpui executor.
///
/// Since #97 an update is the **whole stack**, not just this GUI: the
/// download fetches every binary the release resolved to, and
/// `install_stack` re-verifies *each* one against the embedded key before
/// anything is written. That keeps the path fail-closed per binary even
/// though the check already resolved the assets — nothing rides in
/// unverified behind something else.
///
/// The switch-over is a single atomic pointer move once the whole stack is
/// staged, so "GUI new, daemon stale" is not a reachable state. Nothing is
/// swapped underneath the running processes: the new stack takes effect on
/// the **next launch**, which is why the caller reports "restart to apply"
/// rather than "installed and live".
pub async fn download_and_install(info: wylde_updater::UpdateInfo) -> Result<(), String> {
    wylde_gui_pipe::bridged_spawn_blocking(move || {
        let dl = wylde_updater::download_release(&info).map_err(|e| e.to_string())?;
        wylde_updater::install_stack(&info.version, &dl).map_err(|e| e.to_string())
    })
    .await
}

/// Read the persisted update prefs via the lifecycle pipe.
pub async fn read_update_prefs() -> Result<UpdatePrefs, String> {
    let v = wylde_gui_pipe::lifecycle_action("updater.get_prefs", json!({})).await?;
    Ok(UpdatePrefs::from_value(&v))
}

/// Persist a partial prefs patch.  Mirrors the Svelte `setUpdatePrefs`
/// helper — the lifecycle daemon merges the patch into the on-disk
/// shape and returns the merged result.
pub async fn write_update_prefs(patch: Value) -> Result<UpdatePrefs, String> {
    let v = wylde_gui_pipe::lifecycle_action("updater.set_prefs", patch).await?;
    Ok(UpdatePrefs::from_value(&v))
}

// ── Autostart (Phase 12.3) ───────────────────────────────────────────

/// Wylde's identifier for the `auto-launch` registration.  Same string
/// the Svelte alpha uses so a user who flipped it there sees the
/// same entry when the gpui binary takes over.
pub const AUTOSTART_APP_NAME: &str = "Wylde";

/// True iff the OS reports a login-item under `AUTOSTART_APP_NAME`.
///
/// The bool/Err split lets callers distinguish "off because the user
/// disabled it" from "couldn't ask the OS" — the toggle's UI shows the
/// latter as a side toast with the error message.
pub fn get_autostart_enabled() -> Result<bool, String> {
    autostart_handle()?.is_enabled().map_err(|e| format!("{e}"))
}

pub fn set_autostart_enabled(enabled: bool) -> Result<bool, String> {
    let handle = autostart_handle()?;
    if enabled {
        handle.enable().map_err(|e| format!("{e}"))?;
    } else {
        handle.disable().map_err(|e| format!("{e}"))?;
    }
    handle.is_enabled().map_err(|e| format!("{e}"))
}

fn autostart_handle() -> Result<auto_launch::AutoLaunch, String> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_owned()))
        .unwrap_or_else(|| "wylde-gui.exe".into());
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name(AUTOSTART_APP_NAME)
        .set_app_path(&exe)
        // 0.6 deprecated the `use_launch_agent` bool for an explicit enum; the
        // old `false` mapped to AppleScript mode. macOS-only knob, inert on the
        // Windows target this GUI ships to — kept faithful to the prior value.
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::AppleScript)
        .build()
        .map_err(|e| format!("auto-launch build: {e}"))
}

// ── Ollama defaults ──────────────────────────────────────────────────

/// The persisted Ollama inference defaults, surfaced read-only in the
/// A **sparse** block of the nine Ollama inference fields. Every field is
/// `Option`, so the same type carries two different sparse payloads in the
/// redesign:
///
///   * the model's own declared defaults (`ollama.get_model_defaults`),
///     used as the greyed *placeholder* per field, and
///   * the user's stored *overrides* (`settings.ollama.get_overrides`),
///     used as the normal-weight *value* per field.
///
/// Sparseness is load-bearing: it lets the panel tell "user set
/// temperature = 0.7" apart from "the model default happens to be 0.7".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OllamaSettings {
    pub num_ctx: Option<i64>,
    pub num_predict: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub keep_alive: Option<String>,
}

impl OllamaSettings {
    /// Parse a sparse object of the nine fields. A missing or null key
    /// stays `None`. `keep_alive` accepts a string (`"5m"`, `"-1"`) or a
    /// bare number, matching the values Ollama itself takes.
    pub fn from_value(v: &Value) -> Self {
        let keep_alive = v.get("keep_alive").and_then(|x| match x {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        Self {
            num_ctx: v.get("num_ctx").and_then(|x| x.as_i64()),
            num_predict: v.get("num_predict").and_then(|x| x.as_i64()),
            temperature: v.get("temperature").and_then(|x| x.as_f64()),
            top_p: v.get("top_p").and_then(|x| x.as_f64()),
            top_k: v.get("top_k").and_then(|x| x.as_i64()),
            min_p: v.get("min_p").and_then(|x| x.as_f64()),
            repeat_penalty: v.get("repeat_penalty").and_then(|x| x.as_f64()),
            seed: v.get("seed").and_then(|x| x.as_i64()),
            keep_alive,
        }
    }
}

/// The effective model whose defaults apply to the next chat turn, plus
/// why it was chosen. Mirrors the `models.get_effective` reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveModel {
    /// Resolved model tag, or `None` when nothing is selected.
    pub model: Option<String>,
    /// `"active"` | `"default"` | `"env"` | `None` — provenance of `model`.
    pub source: Option<String>,
}

/// Resolve the effective model via the harness (`models.get_effective`):
/// active inference-bar pick → starred default → `WYLDE_DEFAULT_MODEL` →
/// none. A down harness surfaces as `Err`; the panel then treats it as
/// "no model" (State 1) rather than guessing.
pub async fn read_effective_model() -> Result<EffectiveModel, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({ "action": "models.get_effective", "payload": {} })),
    )
    .await?;
    Ok(EffectiveModel {
        model: v
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        source: v.get("source").and_then(Value::as_str).map(str::to_owned),
    })
}

/// Read a model's own declared inference defaults
/// (`ollama.get_model_defaults`) — sparse, only the keys the model sets.
/// The panel applies the global fallback table for the rest, client-side.
/// Errors propagate verbatim so the panel can branch on
/// `ollama_unreachable` / `model_not_found` (State 2).
pub async fn read_model_defaults(model: &str) -> Result<OllamaSettings, String> {
    let v = wylde_gui_pipe::call(
        "wylde-ollama",
        "POST",
        "/__action__",
        Some(json!({
            "action": "ollama.get_model_defaults",
            "payload": { "model": model },
        })),
    )
    .await?;
    Ok(OllamaSettings::from_value(&v))
}

/// Read the user's *stored* per-model overrides
/// (`settings.ollama.get_overrides`) — sparse, `{}` when none.
pub async fn read_ollama_overrides(model: &str) -> Result<OllamaSettings, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({
            "action": "settings.ollama.get_overrides",
            "payload": { "model": model },
        })),
    )
    .await?;
    let overrides = v.get("overrides").cloned().unwrap_or_else(|| json!({}));
    Ok(OllamaSettings::from_value(&overrides))
}

/// Set/merge a single per-model override key
/// (`settings.ollama.set_overrides`). `value` is any JSON scalar.
pub async fn set_ollama_override(model: &str, key: &str, value: Value) -> Result<(), String> {
    wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({
            "action": "settings.ollama.set_overrides",
            "payload": { "model": model, "key": key, "value": value },
        })),
    )
    .await
    .map(|_| ())
}

/// Clear a single per-model override key (the ↺ reset →
/// `settings.ollama.clear_override`). The field falls back to its
/// placeholder.
pub async fn clear_ollama_override(model: &str, key: &str) -> Result<(), String> {
    wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(json!({
            "action": "settings.ollama.clear_override",
            "payload": { "model": model, "key": key },
        })),
    )
    .await
    .map(|_| ())
}

// ── Voice settings (Slice 6) ──────────────────────────────────────────
//
// The Settings → Voice section reaches the voice service directly over
// `\\.\pipe\wylde-voice` (the `voice.*` action envelope), the same shape
// the consent rows use against `wylde-harness`. The voice service is
// optional: a failed read leaves the section on its defaults plus an
// "offline" note rather than blocking the rest of the panel.

/// Well-known push-to-talk chords, kept as a back-compat seed list and
/// the source of the shipped default (`"Ctrl+Space"`). The picker no
/// longer *cycles* these — the hotkey row is now a live-capture widget
/// (see `crate::hotkey`) — but the list still documents the persisted
/// string style and anchors the default-value invariant in tests.
/// Mirrors `wylde_voice::config_persist::PTT_HOTKEY_PRESETS`.
pub const PTT_HOTKEY_PRESETS: &[&str] =
    &["Ctrl+Space", "Alt+Space", "Right Ctrl", "F8", "CapsLock"];

/// STT backend preference cycle order. Mirrors
/// `wylde_voice::config_persist::ALL_BACKENDS`.
pub const BACKEND_PRESETS: &[&str] = &["auto", "cpu", "npu"];

/// VAD sensitivity cycle order. Mirrors
/// `wylde_voice::config_persist::ALL_VAD_SENSITIVITIES`.
pub const VAD_PRESETS: &[&str] = &["low", "medium", "high"];

/// Wake-word model cycle order. Mirrors
/// `wylde_voice::config_persist::KNOWN_WAKE_WORD_MODELS`.
pub const WAKE_WORD_PRESETS: &[&str] = &[
    "openWakeWord/hey-jarvis",
    "openWakeWord/alexa",
    "openWakeWord/hey-mycroft",
];

/// Sentinel shown in the mic-device picker for "follow the system
/// default input device" (the `input_device == None` state).
pub const DEVICE_SYSTEM_DEFAULT: &str = "System default";

/// The persisted voice config, mirrored from `voice.get_config`. Field
/// set matches `wylde_voice::config_persist::VoiceConfig`.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceSettings {
    /// The capture mode (`config_persist::ALL_MODES`: `push_to_talk` /
    /// `always_on`). Deliberately NOT mirrored as a `MODE_PRESETS` cycle-list:
    /// the panel exposes mode as a two-state toggle, not a cycle picker, so
    /// there is no ordered preset list to keep in lockstep — and
    /// `check-voice-presets-mirror.py` therefore does not (and need not) cover
    /// it. If the panel ever grows a cycle-list over the modes, add a
    /// `MODE_PRESETS` const with a `/// Mirrors …::ALL_MODES` comment and the
    /// gate will pick it up automatically.
    pub mode: String,
    pub push_to_talk_hotkey: String,
    pub stt_backend_pref: String,
    pub vad_sensitivity: String,
    pub wake_word_enabled: bool,
    pub wake_word_model: String,
    /// `None` = follow the system default input device.
    pub input_device: Option<String>,
}

impl Default for VoiceSettings {
    /// Mirrors `VoiceConfig::default()` so the section renders sensibly
    /// before `voice.get_config` returns (or when the voice service is
    /// offline).
    fn default() -> Self {
        Self {
            mode: "push_to_talk".into(),
            push_to_talk_hotkey: "Ctrl+Space".into(),
            stt_backend_pref: "auto".into(),
            vad_sensitivity: "medium".into(),
            wake_word_enabled: false,
            wake_word_model: "openWakeWord/hey-jarvis".into(),
            input_device: None,
        }
    }
}

impl VoiceSettings {
    pub fn from_value(v: &Value) -> Self {
        let d = Self::default();
        Self {
            mode: v
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or(&d.mode)
                .to_owned(),
            push_to_talk_hotkey: v
                .get("push_to_talk_hotkey")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&d.push_to_talk_hotkey)
                .to_owned(),
            stt_backend_pref: v
                .get("stt_backend_pref")
                .and_then(Value::as_str)
                .unwrap_or(&d.stt_backend_pref)
                .to_owned(),
            vad_sensitivity: v
                .get("vad_sensitivity")
                .and_then(Value::as_str)
                .unwrap_or(&d.vad_sensitivity)
                .to_owned(),
            wake_word_enabled: v
                .get("wake_word_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(d.wake_word_enabled),
            wake_word_model: v
                .get("wake_word_model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&d.wake_word_model)
                .to_owned(),
            input_device: v
                .get("input_device")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        }
    }
}

/// Read the persisted voice config (`voice.get_config`).
pub async fn read_voice_settings() -> Result<VoiceSettings, String> {
    let v = voice_action("voice.get_config", json!({})).await?;
    Ok(VoiceSettings::from_value(&v))
}

/// Persist a partial voice-config patch (`voice.set_config`). The daemon
/// merges + validates and returns the full merged config.
pub async fn write_voice_settings(patch: Value) -> Result<VoiceSettings, String> {
    let v = voice_action("voice.set_config", patch).await?;
    Ok(VoiceSettings::from_value(&v))
}

/// Enumerated input devices for the mic picker (`voice.list_input_devices`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceDevices {
    /// The host's default input device name, if any.
    pub default: Option<String>,
    /// All input device names, sorted + de-duplicated by the service.
    pub devices: Vec<String>,
}

/// List input devices for the mic picker.
pub async fn list_input_devices() -> Result<VoiceDevices, String> {
    let v = voice_action("voice.list_input_devices", json!({})).await?;
    let default = v.get("default").and_then(Value::as_str).map(str::to_owned);
    let devices = v
        .get("devices")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(VoiceDevices { default, devices })
}

/// Result of a `voice.test_mic` capture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceTestResult {
    /// RMS level over the capture window, `[0.0, 1.0]` full scale.
    pub rms: f32,
    /// Peak level over the capture window, `[0.0, 1.0]` full scale.
    pub peak: f32,
    /// Best-effort transcript (empty when no STT model is installed).
    pub transcript: String,
    /// A note explaining an empty transcript (no model / no audio).
    pub note: Option<String>,
}

/// View-side state of the "Test mic" button, mirroring [`UpdateCheck`]'s
/// shape. The panel holds one; the section renderer turns it into a
/// status line.
#[derive(Debug, Clone, Default)]
pub enum VoiceTest {
    /// No test run yet this session.
    #[default]
    Idle,
    /// A capture is in flight.
    Running,
    /// Capture finished — carries the level + transcript.
    Done(VoiceTestResult),
    /// The test errored (no device, pipe down); carries the message.
    Failed(String),
}

/// Run a one-off mic test (`voice.test_mic`).
pub async fn test_mic() -> Result<VoiceTestResult, String> {
    let v = voice_action("voice.test_mic", json!({})).await?;
    Ok(VoiceTestResult {
        rms: v.get("rms").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        peak: v.get("peak").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        transcript: v
            .get("transcript")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        note: v.get("note").and_then(Value::as_str).map(str::to_owned),
    })
}

/// Shared envelope helper for the `voice.*` action verbs over
/// `\\.\pipe\wylde-voice`.
async fn voice_action(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-voice",
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

// ── Privacy & Network prefs ───────────────────────────────────────────
//
// The "Privacy & Network" section is the centralized opt-in for features
// that may make an outside network connection (today: HuggingFace online
// model search). Unlike the other sections it does *not* go over a pipe —
// the prefs are a tiny local JSON file owned by the shared Pipe crate's
// `privacy_prefs` cache, so the read/write is synchronous and no backend
// service is in the loop. The Models panel reads the same cache to decide
// whether to surface its HuggingFace affordance.

/// The privacy-prefs shape, re-exported so the panel + its sections can
/// name the type without reaching across crates at every call site.
pub use wylde_gui_pipe::privacy_prefs::PrivacyPrefs;

/// Read the current privacy prefs (cheap copy out of the shared cache).
pub fn read_privacy_prefs() -> PrivacyPrefs {
    wylde_gui_pipe::privacy_prefs::current()
}

/// Persist a new privacy-prefs snapshot (updates the shared cache + the
/// on-disk file). Synchronous — the file is a few bytes.
pub fn write_privacy_prefs(prefs: PrivacyPrefs) -> Result<(), String> {
    wylde_gui_pipe::privacy_prefs::persist(prefs)
}

// ── User profile (Thought Bubble System Slice D) ─────────────────────
//
// The "Profile / Rules" section reads/edits the harness-owned user
// profile and resolves its pending LLM proposals over the in-process
// `user_profile.*` verbs (harness pipe).

/// View-side mirror of the harness `UserProfile`. The three free-text
/// fields the section edits are flattened to `String` (empty = unset);
/// preferences + recurring topics render read-only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserProfile {
    pub name: String,
    pub style: String,
    pub free_text_rules: String,
    /// `(key, value)` preference pairs, shown read-only.
    pub preferences: Vec<(String, String)>,
    pub recurring_topics: Vec<String>,
}

impl UserProfile {
    pub fn from_value(v: &Value) -> Self {
        let preferences = v
            .get("preferences")
            .and_then(|x| x.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let recurring_topics = v
            .get("recurring_topics")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            style: v
                .get("style")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            free_text_rules: v
                .get("free_text_rules")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            preferences,
            recurring_topics,
        }
    }
}

/// One pending LLM-proposed profile update, for the accept/edit/reject UI.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileProposal {
    pub id: String,
    pub field: String,
    pub proposed: String,
    pub current: Option<String>,
    pub rationale: String,
    pub confidence: f64,
}

impl ProfileProposal {
    fn from_value(v: &Value) -> Option<Self> {
        let id = v.get("id").and_then(|x| x.as_str())?.to_owned();
        Some(Self {
            id,
            field: v
                .get("field")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            proposed: v
                .get("proposed")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            current: v.get("current").and_then(|x| x.as_str()).map(str::to_owned),
            rationale: v
                .get("rationale")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            confidence: v.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.0),
        })
    }

    /// `true` when the field is one the section's text inputs can pre-fill
    /// on "Edit" (name / style / free_text_rules).
    pub fn is_text_field(&self) -> bool {
        matches!(self.field.as_str(), "name" | "style" | "free_text_rules")
    }
}

/// Build the `/__action__` envelope for a `user_profile.*` verb. Split
/// out as a pure function so the verb name + payload wiring is unit
/// -testable without a live harness pipe.
pub(crate) fn profile_request(action: &str, payload: Value) -> Value {
    json!({ "action": action, "payload": payload })
}

async fn user_profile_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(profile_request(action, payload)),
    )
    .await
}

/// Read the current user profile (`user_profile.get`).
pub async fn read_user_profile() -> Result<UserProfile, String> {
    let v = user_profile_call("user_profile.get", json!({})).await?;
    Ok(UserProfile::from_value(&v))
}

/// Read the encryption-at-rest toggle (`settings.encryption.get`, OI-14).
/// Served from the harness so the value matches the `data_dir` the stores
/// actually use. Defaults to `true` if the reply omits `enabled`.
pub async fn read_encryption_at_rest() -> Result<bool, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(profile_request("settings.encryption.get", json!({}))),
    )
    .await?;
    Ok(v.get("enabled").and_then(Value::as_bool).unwrap_or(true))
}

/// Persist the encryption-at-rest toggle (`settings.encryption.set`, OI-14).
/// Returns the value the harness stored.
pub async fn write_encryption_at_rest(enabled: bool) -> Result<bool, String> {
    let v = wylde_gui_pipe::call(
        "wylde-harness",
        "POST",
        "/__action__",
        Some(profile_request(
            "settings.encryption.set",
            json!({ "enabled": enabled }),
        )),
    )
    .await?;
    Ok(v.get("enabled").and_then(Value::as_bool).unwrap_or(enabled))
}

/// List pending profile proposals (`user_profile.list_proposals`).
pub async fn list_profile_proposals() -> Result<Vec<ProfileProposal>, String> {
    let v = user_profile_call("user_profile.list_proposals", json!({})).await?;
    let proposals = v
        .get("proposals")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(ProfileProposal::from_value).collect())
        .unwrap_or_default();
    Ok(proposals)
}

/// Apply a user edit to one field (`user_profile.update`). `field` is the
/// profile key; an empty `value` clears name/style and blanks the rules.
pub async fn update_profile_field(field: &str, value: String) -> Result<UserProfile, String> {
    let v = user_profile_call("user_profile.update", json!({ field: value })).await?;
    Ok(UserProfile::from_value(&v))
}

/// Accept a pending proposal (`user_profile.accept`). Returns the updated
/// profile.
pub async fn accept_profile_proposal(id: &str) -> Result<UserProfile, String> {
    let v = user_profile_call("user_profile.accept", json!({ "proposal_id": id })).await?;
    Ok(UserProfile::from_value(&v))
}

/// Reject a pending proposal (`user_profile.reject`).
pub async fn reject_profile_proposal(id: &str) -> Result<(), String> {
    user_profile_call("user_profile.reject", json!({ "proposal_id": id })).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_snapshot_parses_full_payload() {
        let v = json!({
            "no_auth": true,
            "tools": {
                "read_file": "approved",
                "write_file": "denied"
            }
        });
        let snap = ConsentSnapshot::from_value(&v);
        assert!(snap.no_auth);
        assert_eq!(snap.tools.len(), 2);
        assert_eq!(snap.tools.get("read_file").unwrap(), "approved");
        assert_eq!(snap.tools.get("write_file").unwrap(), "denied");
    }

    #[test]
    fn consent_snapshot_handles_empty_object() {
        let snap = ConsentSnapshot::from_value(&json!({}));
        assert!(!snap.no_auth);
        assert!(snap.tools.is_empty());
    }

    #[test]
    fn update_prefs_default_frequency_is_weekly() {
        let p = UpdatePrefs::from_value(&json!({}));
        assert_eq!(p.frequency, "weekly");
        assert!(!p.enabled);
        assert!(!p.auto_check);
        assert_eq!(p.last_checked, None);
        // Privacy-conservative default channel.
        assert_eq!(p.channel, "stable");
        assert_eq!(p.channel(), wylde_updater::Channel::Stable);
    }

    #[test]
    fn update_prefs_parses_beta_channel() {
        let p = UpdatePrefs::from_value(&json!({ "channel": "beta" }));
        assert_eq!(p.channel, "beta");
        assert_eq!(p.channel(), wylde_updater::Channel::Beta);
        // An unknown channel resolves to Stable, never silently to beta.
        let q = UpdatePrefs::from_value(&json!({ "channel": "nightly" }));
        assert_eq!(q.channel(), wylde_updater::Channel::Stable);
    }

    #[test]
    fn autostart_app_name_matches_tauri_alpha() {
        // The Tauri plugin registers under the OS hive using the same
        // name; mismatching here would make the cutover replace a
        // user's existing login item silently and leave the old one
        // pointing at an absent fletch-gui.exe.
        assert_eq!(AUTOSTART_APP_NAME, "Wylde");
    }

    #[test]
    fn ollama_defaults_are_all_none() {
        let d = OllamaSettings::default();
        assert!(d.num_ctx.is_none());
        assert!(d.num_predict.is_none());
        assert!(d.temperature.is_none());
        assert!(d.top_p.is_none());
        assert!(d.top_k.is_none());
        assert!(d.min_p.is_none());
        assert!(d.repeat_penalty.is_none());
        assert!(d.seed.is_none());
        assert!(d.keep_alive.is_none());
    }

    #[test]
    fn ollama_settings_parse_full_gateway_block() {
        // Shape the Gateway's `GET /api/settings/ollama` returns after
        // `wylde_gui_pipe::call` unwraps the `{ok, data}` envelope.
        let v = json!({
            "num_ctx": 8192,
            "num_predict": -1,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "min_p": 0.05,
            "repeat_penalty": 1.1,
            "seed": 0,
            "keep_alive": "5m"
        });
        let o = OllamaSettings::from_value(&v);
        assert_eq!(o.num_ctx, Some(8192));
        assert_eq!(o.num_predict, Some(-1));
        assert_eq!(o.temperature, Some(0.7));
        assert_eq!(o.top_p, Some(0.9));
        assert_eq!(o.top_k, Some(40));
        assert_eq!(o.min_p, Some(0.05));
        assert_eq!(o.repeat_penalty, Some(1.1));
        assert_eq!(o.seed, Some(0));
        assert_eq!(o.keep_alive.as_deref(), Some("5m"));
    }

    #[test]
    fn ollama_settings_keep_alive_accepts_number() {
        // Ollama (and the harness default) also express keep_alive as a
        // bare number of seconds / -1 sentinel.
        let o = OllamaSettings::from_value(&json!({ "keep_alive": -1 }));
        assert_eq!(o.keep_alive.as_deref(), Some("-1"));
    }

    #[test]
    fn voice_settings_parse_full_config_block() {
        let v = json!({
            "mode": "always_on",
            "wake_word_model": "openWakeWord/alexa",
            "wake_word_enabled": true,
            "push_to_talk_hotkey": "F8",
            "stt_backend_pref": "npu",
            "vad_sensitivity": "high",
            "input_device": "USB Mic"
        });
        let s = VoiceSettings::from_value(&v);
        assert_eq!(s.mode, "always_on");
        assert_eq!(s.wake_word_model, "openWakeWord/alexa");
        assert!(s.wake_word_enabled);
        assert_eq!(s.push_to_talk_hotkey, "F8");
        assert_eq!(s.stt_backend_pref, "npu");
        assert_eq!(s.vad_sensitivity, "high");
        assert_eq!(s.input_device.as_deref(), Some("USB Mic"));
    }

    #[test]
    fn voice_settings_missing_keys_fall_back_to_defaults() {
        // A reply missing the Slice-6 keys (e.g. an older service) still
        // yields a fully-populated, sensible struct.
        let s = VoiceSettings::from_value(&json!({ "mode": "push_to_talk" }));
        let d = VoiceSettings::default();
        assert_eq!(s.stt_backend_pref, d.stt_backend_pref);
        assert_eq!(s.vad_sensitivity, d.vad_sensitivity);
        assert_eq!(s.push_to_talk_hotkey, d.push_to_talk_hotkey);
        assert_eq!(s.wake_word_model, d.wake_word_model);
        assert!(!s.wake_word_enabled);
        assert!(s.input_device.is_none());
    }

    #[test]
    fn voice_settings_null_or_empty_device_is_none() {
        let s = VoiceSettings::from_value(&json!({ "input_device": null }));
        assert!(s.input_device.is_none());
        let s = VoiceSettings::from_value(&json!({ "input_device": "" }));
        assert!(s.input_device.is_none());
    }

    #[test]
    fn voice_presets_are_consistent() {
        // Defaults must be members of the cycle lists so the first click
        // advances rather than jumping from an off-list value.
        let d = VoiceSettings::default();
        assert!(BACKEND_PRESETS.contains(&d.stt_backend_pref.as_str()));
        assert!(VAD_PRESETS.contains(&d.vad_sensitivity.as_str()));
        assert!(PTT_HOTKEY_PRESETS.contains(&d.push_to_talk_hotkey.as_str()));
        assert!(WAKE_WORD_PRESETS.contains(&d.wake_word_model.as_str()));
    }

    #[test]
    fn ollama_settings_missing_keys_stay_none() {
        // A partial block (any subset of the Ollama keys) leaves every
        // absent field None so its row renders "—" — the panel never
        // assumes the backend persisted a full settings object.
        let o = OllamaSettings::from_value(&json!({ "temperature": 0.8 }));
        assert_eq!(o.temperature, Some(0.8));
        assert!(o.min_p.is_none());
        assert!(o.num_ctx.is_none());
        assert!(o.keep_alive.is_none());
    }

    // ── User profile (Slice D) ───────────────────────────────────────

    #[test]
    fn user_profile_parses_all_fields() {
        let p = UserProfile::from_value(&json!({
            "name": "Sam",
            "style": "terse",
            "free_text_rules": "Show diffs.",
            "preferences": {"tone": "dry"},
            "recurring_topics": ["rust", "gpui"]
        }));
        assert_eq!(p.name, "Sam");
        assert_eq!(p.style, "terse");
        assert_eq!(p.free_text_rules, "Show diffs.");
        assert_eq!(p.preferences, vec![("tone".to_owned(), "dry".to_owned())]);
        assert_eq!(p.recurring_topics, vec!["rust", "gpui"]);
    }

    #[test]
    fn user_profile_missing_fields_default_empty() {
        let p = UserProfile::from_value(&json!({}));
        assert_eq!(p, UserProfile::default());
    }

    #[test]
    fn proposal_parses_and_classifies_text_field() {
        let p = ProfileProposal::from_value(&json!({
            "id": "abc", "field": "style", "proposed": "terse",
            "current": "verbose", "rationale": "you keep asking", "confidence": 0.9
        }))
        .unwrap();
        assert_eq!(p.id, "abc");
        assert_eq!(p.proposed, "terse");
        assert_eq!(p.current.as_deref(), Some("verbose"));
        assert!(p.is_text_field());

        // A preference field isn't one the inputs pre-fill.
        let pref = ProfileProposal::from_value(&json!({
            "id": "x", "field": "preference:tone", "proposed": "dry", "confidence": 0.8
        }))
        .unwrap();
        assert!(!pref.is_text_field());

        // No id → not a proposal.
        assert!(ProfileProposal::from_value(&json!({"field": "name"})).is_none());
    }

    #[test]
    fn profile_request_wires_verb_and_payload() {
        // The accept/reject wrappers send the proposal id under the
        // `user_profile.{accept,reject}` verbs.
        let acc = profile_request("user_profile.accept", json!({"proposal_id": "p1"}));
        assert_eq!(acc["action"], "user_profile.accept");
        assert_eq!(acc["payload"]["proposal_id"], "p1");

        let rej = profile_request("user_profile.reject", json!({"proposal_id": "p2"}));
        assert_eq!(rej["action"], "user_profile.reject");
        assert_eq!(rej["payload"]["proposal_id"], "p2");

        // update sends the field patch directly.
        let upd = profile_request("user_profile.update", json!({"name": "Sam"}));
        assert_eq!(upd["action"], "user_profile.update");
        assert_eq!(upd["payload"]["name"], "Sam");
    }
}
