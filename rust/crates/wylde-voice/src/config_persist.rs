//! Persistent voice config (mode, push-to-talk hotkey, STT backend
//! preference, mic device, VAD sensitivity, wake-word model). Slice
//! 11.E+ port of `Voice/state.py::VoiceConfig` + `load_config` /
//! `save_config`, extended by Slice 6 (the gpui Settings → Voice
//! surface) with the GUI-editable knobs.
//!
//! Stored as `voice_config.json` at the same path the Python service
//! writes so a flip from `WYLDE_WYLDE_VOICE_IMPL=python` to `rust`
//! carries the user's setting across.
//!
//! Path resolution mirrors Python's:
//!   1. `WYLDE_VOICE_CONFIG_DIR` (explicit override).
//!   2. `WYLDE_DATA_DIR` (the Lifecycle daemon's data root).
//!   3. `<WYLDE_ROOT>/.wylde/data/voice_config.json` (default).
//!
//! The file is small so we use atomic rename for safety: write to
//! `voice_config.json.tmp` then `fs::rename` into place.
//!
//! ## Slice 6 fields are additive + back-compat
//!
//! Every field added past the original `{mode, wake_word_model}` pair
//! carries a `#[serde(default = ...)]`, so a `voice_config.json` written
//! by the pre-Slice-6 service (or by the Python predecessor) still
//! deserialises — the missing keys take their defaults. `normalised`
//! repairs any out-of-range value, so a hand-edited file can never wedge
//! the service.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MODE_PUSH_TO_TALK: &str = "push_to_talk";
pub const MODE_ALWAYS_ON: &str = "always_on";
pub const ALL_MODES: &[&str] = &[MODE_PUSH_TO_TALK, MODE_ALWAYS_ON];

/// Default openWakeWord model. Mirrors
/// [`Voice/state.py::DEFAULT_WAKE_WORD_MODEL`].
pub const DEFAULT_WAKE_WORD_MODEL: &str = "openWakeWord/hey-jarvis";

// ── Slice 6 — GUI-editable knobs ─────────────────────────────────────

/// STT backend preference set by the GUI. `auto` lets the service pick
/// (today: the CPU EP default, per the NPU-spike finding); `cpu` / `npu`
/// pin it. Distinct from the env-only `WYLDE_VOICE_WHISPER_BACKEND` so a
/// GUI choice survives a restart without an env export. Applied on the
/// next service start (the live encoder isn't hot-swapped).
pub const BACKEND_AUTO: &str = "auto";
pub const BACKEND_CPU: &str = "cpu";
pub const BACKEND_NPU: &str = "npu";
pub const ALL_BACKENDS: &[&str] = &[BACKEND_AUTO, BACKEND_CPU, BACKEND_NPU];

/// VAD sensitivity bucket. Maps to the energy+ZCR detector's threshold
/// (see [`vad_threshold_for`]): higher sensitivity → lower threshold →
/// more readily treats quiet input as speech.
pub const VAD_LOW: &str = "low";
pub const VAD_MEDIUM: &str = "medium";
pub const VAD_HIGH: &str = "high";
pub const ALL_VAD_SENSITIVITIES: &[&str] = &[VAD_LOW, VAD_MEDIUM, VAD_HIGH];

/// Default push-to-talk chord. Stored as a human-facing label the GUI
/// shows and a future global-hotkey listener parses.
pub const DEFAULT_PTT_HOTKEY: &str = "Ctrl+Space";

/// The push-to-talk hotkey presets the GUI cycles through. Kept here so
/// the panel and any validator share one list.
pub const PTT_HOTKEY_PRESETS: &[&str] =
    &["Ctrl+Space", "Alt+Space", "Right Ctrl", "F8", "CapsLock"];

/// The openWakeWord bundles the GUI lets the user pick between. Each is
/// resolved by name through `voice.check_wake_word_model` /
/// `voice.pull_wake_word_model`; offering only known-pullable names
/// keeps the picker from selecting a bundle the registry can't fetch.
pub const KNOWN_WAKE_WORD_MODELS: &[&str] = &[
    "openWakeWord/hey-jarvis",
    "openWakeWord/alexa",
    "openWakeWord/hey-mycroft",
];

fn default_wake_word_enabled() -> bool {
    false
}
fn default_ptt_hotkey() -> String {
    DEFAULT_PTT_HOTKEY.to_owned()
}
fn default_backend_pref() -> String {
    BACKEND_AUTO.to_owned()
}
fn default_vad_sensitivity() -> String {
    VAD_MEDIUM.to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceConfig {
    pub mode: String,
    pub wake_word_model: String,
    /// Whether the wake-word listener is armed in always-on mode.
    #[serde(default = "default_wake_word_enabled")]
    pub wake_word_enabled: bool,
    /// Push-to-talk chord label (one of [`PTT_HOTKEY_PRESETS`]).
    #[serde(default = "default_ptt_hotkey")]
    pub push_to_talk_hotkey: String,
    /// STT backend preference (one of [`ALL_BACKENDS`]).
    #[serde(default = "default_backend_pref")]
    pub stt_backend_pref: String,
    /// VAD sensitivity bucket (one of [`ALL_VAD_SENSITIVITIES`]).
    #[serde(default = "default_vad_sensitivity")]
    pub vad_sensitivity: String,
    /// Preferred input device name. `None` (or an empty/absent value)
    /// means "follow the system default input device".
    #[serde(default)]
    pub input_device: Option<String>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            mode: MODE_PUSH_TO_TALK.to_owned(),
            wake_word_model: DEFAULT_WAKE_WORD_MODEL.to_owned(),
            wake_word_enabled: default_wake_word_enabled(),
            push_to_talk_hotkey: default_ptt_hotkey(),
            stt_backend_pref: default_backend_pref(),
            vad_sensitivity: default_vad_sensitivity(),
            input_device: None,
        }
    }
}

impl VoiceConfig {
    /// Coerce values from disk so a corrupted file doesn't leave the
    /// service stuck. Unknown modes fall back to push-to-talk; empty
    /// wake-word strings fall back to the default; out-of-range Slice-6
    /// enums snap to their safe default; an empty `input_device` string
    /// collapses to `None` (system default).
    pub fn normalised(mut self) -> Self {
        if !ALL_MODES.contains(&self.mode.as_str()) {
            self.mode = MODE_PUSH_TO_TALK.to_owned();
        }
        if self.wake_word_model.is_empty() {
            self.wake_word_model = DEFAULT_WAKE_WORD_MODEL.to_owned();
        }
        if self.push_to_talk_hotkey.is_empty() {
            self.push_to_talk_hotkey = DEFAULT_PTT_HOTKEY.to_owned();
        }
        if !ALL_BACKENDS.contains(&self.stt_backend_pref.as_str()) {
            self.stt_backend_pref = BACKEND_AUTO.to_owned();
        }
        if !ALL_VAD_SENSITIVITIES.contains(&self.vad_sensitivity.as_str()) {
            self.vad_sensitivity = VAD_MEDIUM.to_owned();
        }
        if matches!(self.input_device.as_deref(), Some("")) {
            self.input_device = None;
        }
        self
    }

    /// Apply a JSON patch (any subset of the config keys) on top of the
    /// current config and return the re-[`normalised`](Self::normalised)
    /// result. Keys absent from the patch keep their current value;
    /// unknown keys are ignored. `input_device` accepts an explicit
    /// `null` (or empty string) to reset to the system default.
    ///
    /// This is the single write path `voice.set_config` drives, so the
    /// GUI never has to send a full config object — it sends only the
    /// row the user changed.
    pub fn with_patch(mut self, patch: &Value) -> Self {
        if let Some(s) = patch.get("mode").and_then(Value::as_str) {
            self.mode = s.to_owned();
        }
        if let Some(s) = patch.get("wake_word_model").and_then(Value::as_str) {
            self.wake_word_model = s.to_owned();
        }
        if let Some(b) = patch.get("wake_word_enabled").and_then(Value::as_bool) {
            self.wake_word_enabled = b;
        }
        if let Some(s) = patch.get("push_to_talk_hotkey").and_then(Value::as_str) {
            self.push_to_talk_hotkey = s.to_owned();
        }
        if let Some(s) = patch.get("stt_backend_pref").and_then(Value::as_str) {
            self.stt_backend_pref = s.to_owned();
        }
        if let Some(s) = patch.get("vad_sensitivity").and_then(Value::as_str) {
            self.vad_sensitivity = s.to_owned();
        }
        if let Some(v) = patch.get("input_device") {
            self.input_device = match v {
                Value::Null => None,
                Value::String(s) if s.is_empty() => None,
                Value::String(s) => Some(s.clone()),
                // A non-string, non-null value is a malformed patch; keep
                // the current selection rather than corrupting it.
                _ => self.input_device.take(),
            };
        }
        self.normalised()
    }
}

/// Project the persisted VAD sensitivity bucket onto the detector's
/// raw probability threshold. Higher sensitivity → lower threshold, so
/// quieter input still trips speech detection. `medium` reproduces the
/// service's built-in [`crate::vad::DEFAULT_THRESHOLD`] (0.65).
pub fn vad_threshold_for(sensitivity: &str) -> f32 {
    match sensitivity {
        VAD_HIGH => 0.45,
        VAD_LOW => 0.80,
        // medium / anything unexpected → the proven default.
        _ => crate::vad::DEFAULT_THRESHOLD,
    }
}

/// Resolve the config file path. Honours the same env vars Python uses
/// so a side-by-side strangler-fig flip reads/writes the same file.
pub fn config_path() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_VOICE_CONFIG_DIR") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p.join("voice_config.json");
        }
    }
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p.join("voice_config.json");
        }
    }
    let wylde_root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    wylde_root
        .join(".wylde")
        .join("data")
        .join("voice_config.json")
}

/// Load the config file, returning defaults on any error.
pub fn load_config() -> VoiceConfig {
    load_config_at(&config_path())
}

pub fn load_config_at(path: &Path) -> VoiceConfig {
    let Ok(bytes) = std::fs::read(path) else {
        return VoiceConfig::default();
    };
    match serde_json::from_slice::<VoiceConfig>(&bytes) {
        Ok(c) => c.normalised(),
        Err(e) => {
            tracing::warn!(
                "wylde-voice: config unreadable at {} ({e}); using defaults",
                path.display()
            );
            VoiceConfig::default()
        }
    }
}

/// Persist the config atomically. Surface the IO error to the caller —
/// the action handler logs and continues rather than failing the call
/// because a transient write failure shouldn't block a mode toggle.
pub fn save_config(cfg: &VoiceConfig) -> std::io::Result<()> {
    save_config_at(cfg, &config_path())
}

pub fn save_config_at(cfg: &VoiceConfig, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(cfg).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_round_trips() {
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.mode, MODE_PUSH_TO_TALK);
        assert_eq!(cfg.wake_word_model, DEFAULT_WAKE_WORD_MODEL);
    }

    #[test]
    fn normalised_repairs_bad_mode() {
        let cfg = VoiceConfig {
            mode: "elixir-mode".to_owned(),
            wake_word_model: "openWakeWord/alexa".to_owned(),
            ..VoiceConfig::default()
        }
        .normalised();
        assert_eq!(cfg.mode, MODE_PUSH_TO_TALK);
        assert_eq!(cfg.wake_word_model, "openWakeWord/alexa");
    }

    #[test]
    fn normalised_fills_empty_wake_word() {
        let cfg = VoiceConfig {
            mode: MODE_ALWAYS_ON.to_owned(),
            wake_word_model: String::new(),
            ..VoiceConfig::default()
        }
        .normalised();
        assert_eq!(cfg.mode, MODE_ALWAYS_ON);
        assert_eq!(cfg.wake_word_model, DEFAULT_WAKE_WORD_MODEL);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("voice_config.json");
        let cfg = VoiceConfig {
            mode: MODE_ALWAYS_ON.to_owned(),
            wake_word_model: "openWakeWord/computer".to_owned(),
            wake_word_enabled: true,
            push_to_talk_hotkey: "F8".to_owned(),
            stt_backend_pref: BACKEND_NPU.to_owned(),
            vad_sensitivity: VAD_HIGH.to_owned(),
            input_device: Some("Headset Mic".to_owned()),
        };
        save_config_at(&cfg, &path).unwrap();
        let loaded = load_config_at(&path);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn defaults_fill_slice6_fields() {
        let cfg = VoiceConfig::default();
        assert!(!cfg.wake_word_enabled);
        assert_eq!(cfg.push_to_talk_hotkey, DEFAULT_PTT_HOTKEY);
        assert_eq!(cfg.stt_backend_pref, BACKEND_AUTO);
        assert_eq!(cfg.vad_sensitivity, VAD_MEDIUM);
        assert!(cfg.input_device.is_none());
    }

    #[test]
    fn legacy_two_field_json_deserialises_with_defaults() {
        // A voice_config.json written before Slice 6 has only the
        // original two keys; serde(default) fills the rest.
        let v = r#"{"mode":"always_on","wake_word_model":"openWakeWord/alexa"}"#;
        let cfg: VoiceConfig = serde_json::from_str(v).unwrap();
        let cfg = cfg.normalised();
        assert_eq!(cfg.mode, MODE_ALWAYS_ON);
        assert_eq!(cfg.wake_word_model, "openWakeWord/alexa");
        assert_eq!(cfg.stt_backend_pref, BACKEND_AUTO);
        assert_eq!(cfg.vad_sensitivity, VAD_MEDIUM);
        assert_eq!(cfg.push_to_talk_hotkey, DEFAULT_PTT_HOTKEY);
        assert!(!cfg.wake_word_enabled);
        assert!(cfg.input_device.is_none());
    }

    #[test]
    fn normalised_repairs_slice6_enums() {
        let cfg = VoiceConfig {
            stt_backend_pref: "quantum".to_owned(),
            vad_sensitivity: "extreme".to_owned(),
            push_to_talk_hotkey: String::new(),
            input_device: Some(String::new()),
            ..VoiceConfig::default()
        }
        .normalised();
        assert_eq!(cfg.stt_backend_pref, BACKEND_AUTO);
        assert_eq!(cfg.vad_sensitivity, VAD_MEDIUM);
        assert_eq!(cfg.push_to_talk_hotkey, DEFAULT_PTT_HOTKEY);
        assert!(cfg.input_device.is_none());
    }

    #[test]
    fn with_patch_applies_subset_and_normalises() {
        let cfg = VoiceConfig::default()
            .with_patch(&serde_json::json!({
                "mode": "always_on",
                "stt_backend_pref": "npu",
                "vad_sensitivity": "high",
                "wake_word_enabled": true,
                "input_device": "USB Mic",
            }));
        assert_eq!(cfg.mode, MODE_ALWAYS_ON);
        assert_eq!(cfg.stt_backend_pref, BACKEND_NPU);
        assert_eq!(cfg.vad_sensitivity, VAD_HIGH);
        assert!(cfg.wake_word_enabled);
        assert_eq!(cfg.input_device.as_deref(), Some("USB Mic"));
        // Untouched key keeps its default.
        assert_eq!(cfg.push_to_talk_hotkey, DEFAULT_PTT_HOTKEY);
    }

    #[test]
    fn with_patch_null_input_device_resets_to_default() {
        let cfg = VoiceConfig {
            input_device: Some("Old Mic".to_owned()),
            ..VoiceConfig::default()
        };
        let cleared = cfg.with_patch(&serde_json::json!({ "input_device": null }));
        assert!(cleared.input_device.is_none());
    }

    #[test]
    fn with_patch_rejects_bad_enum_by_normalising() {
        // A patch with an out-of-range enum doesn't wedge the config —
        // normalisation snaps it back to the safe default.
        let cfg = VoiceConfig::default()
            .with_patch(&serde_json::json!({ "stt_backend_pref": "tpu" }));
        assert_eq!(cfg.stt_backend_pref, BACKEND_AUTO);
    }

    #[test]
    fn vad_threshold_buckets_are_monotonic() {
        let high = vad_threshold_for(VAD_HIGH);
        let medium = vad_threshold_for(VAD_MEDIUM);
        let low = vad_threshold_for(VAD_LOW);
        // Higher sensitivity → lower threshold.
        assert!(high < medium, "{high} !< {medium}");
        assert!(medium < low, "{medium} !< {low}");
        // medium reproduces the service default exactly.
        assert_eq!(medium, crate::vad::DEFAULT_THRESHOLD);
        // Unknown bucket falls back to medium/default.
        assert_eq!(vad_threshold_for("nonsense"), crate::vad::DEFAULT_THRESHOLD);
    }

    #[test]
    fn load_missing_returns_default() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("does-not-exist.json");
        assert_eq!(load_config_at(&path), VoiceConfig::default());
    }

    #[test]
    fn load_corrupt_returns_default() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("voice_config.json");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load_config_at(&path), VoiceConfig::default());
    }

    #[test]
    fn save_is_atomic_via_rename() {
        // We can't easily inject a write failure, but we can at least
        // verify the side-product file isn't left behind on success.
        let td = TempDir::new().unwrap();
        let path = td.path().join("voice_config.json");
        save_config_at(&VoiceConfig::default(), &path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }
}
