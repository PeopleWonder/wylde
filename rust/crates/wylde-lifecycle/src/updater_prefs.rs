//! Self-updater preferences (`updater.get_prefs` / `updater.set_prefs`).
//!
//! Phase 12.5 wired the Settings → Updates section to dispatch
//! `updater.get_prefs` / `updater.set_prefs` against the **lifecycle**
//! daemon (`wylde_gui_pipe::lifecycle_action`), but the handlers were
//! never registered — every toggle landed on `no_action` and the read
//! silently fell back to hard-coded defaults, so the section never
//! persisted anything and the startup auto-check could never record its
//! `last_checked` stamp. This module is the missing daemon side.
//!
//! The on-disk shape is a tiny JSON file under the same data root every
//! other lifecycle-adjacent service uses (`WYLDE_DATA_DIR` →
//! `WYLDE_ROOT/.wylde/data`), mirroring `wylde-voice`'s `config_persist`
//! pattern (atomic temp-write + rename, defaults on any read error). The
//! merged shape the handlers return is exactly what the GUI's
//! `UpdatePrefs::from_value` parses: `{enabled, auto_check, frequency,
//! channel, last_checked}`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Persisted updater preferences. Field set + defaults match the GUI's
/// `UpdatePrefs` (Settings panel `ipc.rs`) so a round-trip is lossless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdaterPrefs {
    /// Master switch for the whole update feature.
    pub enabled: bool,
    /// Run a background check on startup / on the chosen cadence.
    pub auto_check: bool,
    /// Cadence string — `"daily"` / `"weekly"` / `"monthly"`.
    pub frequency: String,
    /// Release channel — `"stable"` / `"beta"`.
    pub channel: String,
    /// Unix-seconds timestamp of the last completed check, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked: Option<u64>,
    /// A specific version the user chose to skip ("Decline" on the
    /// changelog card). The auto-check path suppresses this exact version
    /// so it stops re-prompting; a *newer* release has a different version
    /// string and so is offered normally (the skip self-expires). `None`
    /// once a newer version supersedes it or the user never skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_version: Option<String>,
}

impl Default for UpdaterPrefs {
    /// Privacy-conservative baseline: feature off, weekly/stable. Same
    /// values the GUI assumes when the read fails, so a missing file and
    /// a successful read of a fresh file render identically.
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

impl UpdaterPrefs {
    /// Serialise to the wire/JSON object the GUI parses.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }

    /// Merge a partial patch in place. Only keys present in `patch` are
    /// touched; unknown keys are ignored (forward-compat). Invalid types
    /// for a known key are skipped rather than clobbering a good value.
    pub fn apply_patch(&mut self, patch: &Value) {
        let Some(obj) = patch.as_object() else {
            return;
        };
        if let Some(v) = obj.get("enabled").and_then(Value::as_bool) {
            self.enabled = v;
        }
        if let Some(v) = obj.get("auto_check").and_then(Value::as_bool) {
            self.auto_check = v;
        }
        if let Some(v) = obj.get("frequency").and_then(Value::as_str) {
            self.frequency = v.to_owned();
        }
        if let Some(v) = obj.get("channel").and_then(Value::as_str) {
            self.channel = v.to_owned();
        }
        // `last_checked` is settable to a number (the startup check stamps
        // it) or explicitly cleared with JSON null.
        match obj.get("last_checked") {
            Some(Value::Number(n)) => self.last_checked = n.as_u64(),
            Some(Value::Null) => self.last_checked = None,
            _ => {}
        }
        // `skipped_version` is a version string (the "Decline" click) or an
        // explicit JSON null to un-skip.
        match obj.get("skipped_version") {
            Some(Value::String(s)) => self.skipped_version = Some(s.clone()),
            Some(Value::Null) => self.skipped_version = None,
            _ => {}
        }
    }
}

/// Resolve the prefs file path. Honours the same env-var ladder the rest
/// of the data layer uses so every component reads/writes one location.
pub fn prefs_path() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p.join("updater_prefs.json");
        }
    }
    let wylde_root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    wylde_root
        .join(".wylde")
        .join("data")
        .join("updater_prefs.json")
}

/// Load prefs, returning defaults on any error (missing file, bad JSON).
pub fn load() -> UpdaterPrefs {
    load_at(&prefs_path())
}

pub fn load_at(path: &Path) -> UpdaterPrefs {
    let Ok(bytes) = std::fs::read(path) else {
        return UpdaterPrefs::default();
    };
    match serde_json::from_slice::<UpdaterPrefs>(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "wylde-lifecycle: updater_prefs unreadable at {} ({e}); using defaults",
                path.display()
            );
            UpdaterPrefs::default()
        }
    }
}

/// Persist prefs atomically (temp-write + rename).
pub fn save(prefs: &UpdaterPrefs) -> std::io::Result<()> {
    save_at(prefs, &prefs_path())
}

pub fn save_at(prefs: &UpdaterPrefs, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(prefs).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_matches_gui_baseline() {
        let p = UpdaterPrefs::default();
        assert!(!p.enabled);
        assert!(!p.auto_check);
        assert_eq!(p.frequency, "weekly");
        assert_eq!(p.channel, "stable");
        assert_eq!(p.last_checked, None);
    }

    #[test]
    fn missing_file_loads_defaults() {
        let td = TempDir::new().unwrap();
        let p = load_at(&td.path().join("nope.json"));
        assert_eq!(p, UpdaterPrefs::default());
    }

    #[test]
    fn patch_merges_only_present_keys() {
        let mut p = UpdaterPrefs::default();
        p.apply_patch(&json!({ "enabled": true, "channel": "beta" }));
        assert!(p.enabled);
        assert_eq!(p.channel, "beta");
        // Untouched keys keep their value.
        assert!(!p.auto_check);
        assert_eq!(p.frequency, "weekly");
    }

    #[test]
    fn patch_ignores_bad_types_and_unknown_keys() {
        let mut p = UpdaterPrefs::default();
        p.apply_patch(&json!({ "enabled": "yes", "frequency": 5, "bogus": true }));
        // String for a bool key and number for a string key are both skipped.
        assert!(!p.enabled);
        assert_eq!(p.frequency, "weekly");
    }

    #[test]
    fn skipped_version_set_and_cleared() {
        let mut p = UpdaterPrefs::default();
        assert_eq!(p.skipped_version, None);
        // The "Decline" click records the exact version.
        p.apply_patch(&json!({ "skipped_version": "0.3.1" }));
        assert_eq!(p.skipped_version.as_deref(), Some("0.3.1"));
        // A superseding write / un-skip clears it with null.
        p.apply_patch(&json!({ "skipped_version": null }));
        assert_eq!(p.skipped_version, None);
    }

    #[test]
    fn skipped_version_round_trips_through_disk() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("updater_prefs.json");
        let mut p = UpdaterPrefs::default();
        p.apply_patch(&json!({ "skipped_version": "1.4.0" }));
        save_at(&p, &path).unwrap();
        assert_eq!(load_at(&path).skipped_version.as_deref(), Some("1.4.0"));
    }

    #[test]
    fn last_checked_set_and_cleared() {
        let mut p = UpdaterPrefs::default();
        p.apply_patch(&json!({ "last_checked": 1_700_000_000_u64 }));
        assert_eq!(p.last_checked, Some(1_700_000_000));
        p.apply_patch(&json!({ "last_checked": null }));
        assert_eq!(p.last_checked, None);
    }

    #[test]
    fn round_trips_through_disk() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("updater_prefs.json");
        let mut p = UpdaterPrefs::default();
        p.apply_patch(&json!({ "enabled": true, "auto_check": true, "frequency": "daily" }));
        save_at(&p, &path).unwrap();
        let back = load_at(&path);
        assert_eq!(p, back);
    }

    #[test]
    fn to_value_carries_every_field_the_gui_reads() {
        let mut p = UpdaterPrefs::default();
        p.apply_patch(&json!({ "last_checked": 42_u64 }));
        let v = p.to_value();
        assert_eq!(v["enabled"], json!(false));
        assert_eq!(v["auto_check"], json!(false));
        assert_eq!(v["frequency"], json!("weekly"));
        assert_eq!(v["channel"], json!("stable"));
        assert_eq!(v["last_checked"], json!(42));
    }
}
