//! Privacy & Network preferences — the centralized opt-in store for
//! features that may make an outside network connection.
//!
//! Privacy-first: every flag defaults **off**.  The store is a tiny JSON
//! file at `$WYLDE_ROOT/data/settings/privacy.json`, written entirely in
//! Rust with no backend service in the loop (no pipe round-trip, no
//! Python).  It lives here in the Pipe crate because it is the one crate
//! both the Settings panel (the *writer*) and the Models panel (the
//! *reader*) already depend on.
//!
//! A process-global cache keeps the two panels in sync without an event
//! bus: the Settings panel persists a change → the cache updates in the
//! same call → the Models panel reads the fresh value on its next render.
//! Panels are long-lived once mounted (the Shell caches the View), so the
//! Models panel can't rely on a one-shot read at construction — it reads
//! [`current`] each render, which is a cheap copy out of the cache.
//!
//! The on-disk shape mirrors the file `$WYLDE_ROOT/data/settings/`
//! convention the Gateway's `ollama.json` already uses, so the privacy
//! prefs sit alongside the other settings rather than in a new location.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// The centralized opt-in flags for "may make an outside connection"
/// features.  Two booleans today (HuggingFace online model search);
/// future privacy-gated features add their flag here and the file format
/// grows a key — a reader on the old format just sees the new key default
/// to `false`, which is the privacy-safe direction.
///
/// `Copy` because it is two bits — the panels keep their own mirror and
/// pass it around by value rather than threading a borrow of the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrivacyPrefs {
    /// Opt-in to querying HuggingFace's public model API from the Models
    /// panel.  Off → the "Search HuggingFace" affordance never appears and
    /// no outside connection is ever attempted.
    pub hf_search_enabled: bool,
    /// True once the first-time HuggingFace privacy warning has been shown
    /// and acknowledged.  Resettable from Settings ("Reset privacy
    /// warnings") so a user can surface the warning again on demand.
    pub hf_search_warning_shown: bool,
}

impl PrivacyPrefs {
    /// Parse the on-disk shape.  Every flag is optional and defaults to
    /// `false` — a missing key, an empty object, or a malformed value all
    /// resolve to the privacy-safe "off" state.
    pub fn from_value(v: &Value) -> Self {
        Self {
            hf_search_enabled: v
                .get("hf_search_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hf_search_warning_shown: v
                .get("hf_search_warning_shown")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// Serialise to the on-disk JSON shape.
    pub fn to_value(&self) -> Value {
        json!({
            "hf_search_enabled": self.hf_search_enabled,
            "hf_search_warning_shown": self.hf_search_warning_shown,
        })
    }
}

/// Resolve `$WYLDE_ROOT/data/settings/privacy.json`.  Mirrors the
/// `manifest_dir`/Gateway-settings convention (root from `WYLDE_ROOT`,
/// defaulting to `.`), read on every call so tests can point the var at a
/// scratch dir.
fn prefs_path() -> PathBuf {
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("data").join("settings").join("privacy.json")
}

/// Read the prefs from a specific path.  Any failure (missing file, bad
/// JSON) yields the all-`false` default rather than erroring — a fresh
/// install has no file yet, and a corrupt file must never fail *open* on
/// a privacy flag.
fn read_from_path(path: &Path) -> PrivacyPrefs {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<Value>(&s)
            .map(|v| PrivacyPrefs::from_value(&v))
            .unwrap_or_default(),
        Err(_) => PrivacyPrefs::default(),
    }
}

/// Write the prefs to a specific path, creating the parent dir.  Writes to
/// a sibling `.tmp` then renames so a crash mid-write can't leave a
/// half-written (and thus parse-failing → fail-open-to-default) file.
fn write_to_path(path: &Path, prefs: &PrivacyPrefs) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("privacy prefs: mkdir: {e}"))?;
    }
    let body = serde_json::to_vec_pretty(&prefs.to_value())
        .map_err(|e| format!("privacy prefs: encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("privacy prefs: write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("privacy prefs: rename: {e}"))?;
    Ok(())
}

/// Process-global cache, lazily seeded from disk on first access.
static CACHE: OnceLock<Mutex<PrivacyPrefs>> = OnceLock::new();

fn cache() -> &'static Mutex<PrivacyPrefs> {
    CACHE.get_or_init(|| Mutex::new(read_from_path(&prefs_path())))
}

/// Current snapshot — a cheap copy out of the in-memory cache (seeded from
/// disk on first access).  Safe to call every render.
pub fn current() -> PrivacyPrefs {
    *cache().lock().unwrap_or_else(|e| e.into_inner())
}

/// Persist a new snapshot: update the cache **and** write it to disk.
///
/// The cache is updated even when the disk write fails, so the in-session
/// UI stays consistent with what the user just chose; the `Err` is handed
/// back to the caller to surface in a banner.  This mirrors the optimistic
/// write model the rest of the Settings panel uses.
pub fn persist(next: PrivacyPrefs) -> Result<(), String> {
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = next;
    write_to_path(&prefs_path(), &next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_off() {
        let p = PrivacyPrefs::default();
        assert!(!p.hf_search_enabled);
        assert!(!p.hf_search_warning_shown);
    }

    #[test]
    fn from_value_defaults_missing_keys_to_off() {
        // Empty object → both off (fresh install / older file).
        let p = PrivacyPrefs::from_value(&json!({}));
        assert!(!p.hf_search_enabled);
        assert!(!p.hf_search_warning_shown);
        // A partial object only flips the key it carries.
        let p = PrivacyPrefs::from_value(&json!({ "hf_search_enabled": true }));
        assert!(p.hf_search_enabled);
        assert!(!p.hf_search_warning_shown);
    }

    #[test]
    fn from_value_tolerates_wrong_types() {
        // A malformed value must fail *closed* (off), never open.
        let p = PrivacyPrefs::from_value(&json!({ "hf_search_enabled": "yes" }));
        assert!(!p.hf_search_enabled);
    }

    #[test]
    fn value_round_trips() {
        let p = PrivacyPrefs {
            hf_search_enabled: true,
            hf_search_warning_shown: true,
        };
        let back = PrivacyPrefs::from_value(&p.to_value());
        assert_eq!(p, back);
    }

    #[test]
    fn disk_round_trip_through_path() {
        // Hermetic: write + read a scratch path directly, bypassing the
        // process-global cache (which other tests in the binary share).
        let dir = std::env::temp_dir().join(format!(
            "wylde-privacy-test-{}",
            std::process::id(),
        ));
        let path = dir.join("data").join("settings").join("privacy.json");
        // Missing file → default.
        assert_eq!(read_from_path(&path), PrivacyPrefs::default());
        let prefs = PrivacyPrefs {
            hf_search_enabled: true,
            hf_search_warning_shown: false,
        };
        write_to_path(&path, &prefs).expect("write");
        assert_eq!(read_from_path(&path), prefs);
        // No leftover temp file after a successful rename.
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_reads_as_default() {
        let dir = std::env::temp_dir().join(format!(
            "wylde-privacy-corrupt-{}",
            std::process::id(),
        ));
        let path = dir.join("privacy.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ not json ]").unwrap();
        assert_eq!(read_from_path(&path), PrivacyPrefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
