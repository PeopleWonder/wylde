//! The master toggle -- `HierarchyConfig` (definitional-hierarchy plan SS4,
//! "Master on/off toggle. Reuse the `wylde-concept-routing` config pattern
//! exactly ... `<data_dir>/settings/`-backed, fail-closed to OFF").
//!
//! A faithful, minimal clone of `wylde_concept_routing::config`: a process-global
//! `OnceLock<Mutex<_>>` lazily seeded from disk, a [`current`](HierarchyConfig::current)
//! snapshot read, and an optimistic [`persist`](HierarchyConfig::persist) that
//! updates the cache even when the disk write fails. The on-disk file is
//! `<data_dir>/settings/hierarchy.json`, alongside `concept_routing.json` and the
//! other settings stores.
//!
//! **Fail-safe direction is OFF.** A missing file, a corrupt file, or a malformed
//! value all resolve to [`HierarchyConfig::default`], whose
//! [`enabled`](HierarchyConfig::enabled) is `false`. In H0 the only consumer of
//! the toggle is [`crate::build_view_if_enabled`], which returns `None` when off
//! -- so "OFF => inert" holds: a toggle-respecting caller gets nothing and
//! today's behaviour is byte-identical. The injection + spread + sub-tab seams
//! the same flag will gate are later slices (H2/H5/H6).
//!
//! ## One toggle (H0)
//!
//! H0 ships the single `enabled` master flag (plan SS4 default; OQ-7 -- one
//! toggle vs two -- is an open question deferred to when the retrieval seams
//! land). The struct round-trips through JSON with `#[serde(default)]`, so a
//! later split into independent "show" vs "affect retrieval" flags is additive.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The master hierarchy config. Default is OFF (today's exact behaviour) -- the
/// derived `Default` gives `enabled: false`; the feature can only ever be
/// *added* by an explicit, persisted opt-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HierarchyConfig {
    /// **THE MASTER TOGGLE.** `false` => the hierarchy projection is never
    /// surfaced and every gated seam is inert. Default `false`.
    #[serde(default)]
    pub enabled: bool,
}

impl HierarchyConfig {
    /// Parse the on-disk shape. Tolerant: a non-object, a missing key, or a
    /// wrong-typed value all fall back to the default (OFF), so a degraded file
    /// keeps the feature off, never silently on.
    pub fn from_value(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Serialise to the on-disk JSON shape.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

// `<data_dir>` (convention A: `WYLDE_DATA_DIR` -> `DATA_DIR` ->
// `<WYLDE_ROOT>/.wylde/data`) from the ONE canonical resolver (#138) -- this was
// a verbatim copy of that body.
use wylde_shared::paths::data_dir;

/// `<data_dir>/settings/hierarchy.json` -- alongside `concept_routing.json`.
fn config_path() -> PathBuf {
    data_dir().join("settings").join("hierarchy.json")
}

/// Read the config from a path. Any failure (missing file, bad JSON) yields the
/// default (OFF) rather than erroring -- a fresh install has no file, and a
/// corrupt file must fail *closed*, never on.
fn read_from_path(path: &std::path::Path) -> HierarchyConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<Value>(&s)
            .map(|v| HierarchyConfig::from_value(&v))
            .unwrap_or_default(),
        Err(_) => HierarchyConfig::default(),
    }
}

/// Write the config to a path, creating the parent dir. Writes a sibling `.tmp`
/// then renames so a crash mid-write can't leave a half-written (parse-failing
/// => fail-off) file. Mirrors the routing-config writer.
fn write_to_path(path: &std::path::Path, cfg: &HierarchyConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("hierarchy: mkdir: {e}"))?;
    }
    let body = serde_json::to_vec_pretty(&cfg.to_value())
        .map_err(|e| format!("hierarchy: encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("hierarchy: write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("hierarchy: rename: {e}"))?;
    Ok(())
}

/// Process-global cache, lazily seeded from disk on first access.
static CACHE: OnceLock<Mutex<HierarchyConfig>> = OnceLock::new();

fn cache() -> &'static Mutex<HierarchyConfig> {
    CACHE.get_or_init(|| Mutex::new(read_from_path(&config_path())))
}

impl HierarchyConfig {
    /// Current snapshot -- a cheap copy out of the in-memory cache (seeded from
    /// disk on first access). Safe on the per-turn hot path.
    pub fn current() -> HierarchyConfig {
        *cache().lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist a new snapshot: update the cache AND write it to disk. The cache
    /// updates even when the disk write fails (optimistic write), so the
    /// in-session behaviour matches what the user just chose; the `Err` is
    /// handed back to surface in a banner.
    pub fn persist(next: HierarchyConfig) -> Result<(), String> {
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = next;
        write_to_path(&config_path(), &next)
    }

    /// Force-refresh the cache from disk (for a test or an out-of-band writer).
    pub fn reload_from_disk() -> HierarchyConfig {
        let fresh = read_from_path(&config_path());
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn default_is_off() {
        assert!(
            !HierarchyConfig::default().enabled,
            "master toggle defaults OFF"
        );
    }

    #[test]
    fn missing_or_malformed_fails_closed_to_off() {
        // Empty object (fresh install / older file) -> off.
        assert!(!HierarchyConfig::from_value(&json!({})).enabled);
        // Garbage typed value -> off, never on.
        assert!(!HierarchyConfig::from_value(&json!({ "enabled": "yes" })).enabled);
        // An explicit on round-trips.
        let c = HierarchyConfig { enabled: true };
        assert_eq!(HierarchyConfig::from_value(&c.to_value()), c);
    }

    #[test]
    fn disk_round_trip_through_path() {
        let dir = std::env::temp_dir().join(format!("wylde-hier-cfg-{}", std::process::id()));
        let path = dir.join("settings").join("hierarchy.json");
        // Missing file -> default (off).
        assert_eq!(read_from_path(&path), HierarchyConfig::default());
        let cfg = HierarchyConfig { enabled: true };
        write_to_path(&path, &cfg).expect("write");
        assert_eq!(read_from_path(&path), cfg);
        assert!(!path.with_extension("json.tmp").exists(), "no leftover tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_reads_as_default_off() {
        let dir = std::env::temp_dir().join(format!("wylde-hier-corrupt-{}", std::process::id()));
        let path = dir.join("hierarchy.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ not json ]").unwrap();
        assert!(!read_from_path(&path).enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn current_and_persist_through_env_dir() {
        // Drive the real cache + path resolution via WYLDE_DATA_DIR. Serial:
        // mutates a process-global env var + the shared cache.
        let dir = std::env::temp_dir().join(format!("wylde-hier-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("WYLDE_DATA_DIR", &dir);

        // Fresh: reload picks up the absent file as default-off.
        assert!(!HierarchyConfig::reload_from_disk().enabled);

        // Persist on, then reload proves it stuck.
        HierarchyConfig::persist(HierarchyConfig { enabled: true }).expect("persist");
        assert!(HierarchyConfig::current().enabled);
        assert!(HierarchyConfig::reload_from_disk().enabled);

        std::env::remove_var("WYLDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        // Restore the cache to default so later tests aren't left seeing on.
        HierarchyConfig::reload_from_disk();
    }
}
