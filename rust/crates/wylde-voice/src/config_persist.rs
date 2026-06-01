//! Persistent voice config (push-to-talk vs always-on, wake-word model
//! name). Slice 11.E+ port of `Voice/state.py::VoiceConfig` +
//! `load_config` / `save_config`.
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
//! The file is small (<100 bytes) so we use atomic rename for safety:
//! write to `voice_config.json.tmp` then `fs::rename` into place.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MODE_PUSH_TO_TALK: &str = "push_to_talk";
pub const MODE_ALWAYS_ON: &str = "always_on";
pub const ALL_MODES: &[&str] = &[MODE_PUSH_TO_TALK, MODE_ALWAYS_ON];

/// Default openWakeWord model. Mirrors
/// [`Voice/state.py::DEFAULT_WAKE_WORD_MODEL`].
pub const DEFAULT_WAKE_WORD_MODEL: &str = "openWakeWord/hey-jarvis";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceConfig {
    pub mode: String,
    pub wake_word_model: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            mode: MODE_PUSH_TO_TALK.to_owned(),
            wake_word_model: DEFAULT_WAKE_WORD_MODEL.to_owned(),
        }
    }
}

impl VoiceConfig {
    /// Coerce values from disk so a corrupted file doesn't leave the
    /// service stuck. Unknown modes fall back to push-to-talk; empty
    /// wake-word strings fall back to the default.
    pub fn normalised(mut self) -> Self {
        if !ALL_MODES.contains(&self.mode.as_str()) {
            self.mode = MODE_PUSH_TO_TALK.to_owned();
        }
        if self.wake_word_model.is_empty() {
            self.wake_word_model = DEFAULT_WAKE_WORD_MODEL.to_owned();
        }
        self
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
        };
        save_config_at(&cfg, &path).unwrap();
        let loaded = load_config_at(&path);
        assert_eq!(loaded, cfg);
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
