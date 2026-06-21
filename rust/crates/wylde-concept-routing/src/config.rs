//! The master toggle + all routing knobs — `RoutingConfig` (concept-routing
//! plan §3, requirement 1).
//!
//! **Harness-owned, read in-process.** The *consumer* is the harness gather
//! (and, server-side, the workspaces routing bridge); the *writer* is the GUI
//! through the `settings.concept_routing.{get,set}` facade verbs. One store
//! read in-process by both — deliberately **not** a second source of truth —
//! so we avoid the TCP↔pipe drift trap the Ollama settings already hit
//! (memory `wylde-settings-ollama-defaults-ux-scope`).
//!
//! The cache + persistence shape is a faithful clone of the privacy-prefs
//! store (`Core/GUI/Frontend/Pipe/src/privacy_prefs.rs`): a process-global
//! `OnceLock<Mutex<_>>` lazily seeded from disk, a `current()` snapshot read,
//! and an optimistic `persist()` that updates the cache even when the disk
//! write fails. The on-disk file is `<data_dir>/settings/concept_routing.json`.
//!
//! **Fail-safe direction is OFF.** A missing file, a corrupt file, or a
//! malformed value all resolve to [`RoutingConfig::default`], whose
//! [`RoutingConfig::enabled`] is `false` — i.e. today's exact RAG behaviour.
//! Routing can only ever be *added* by an explicit, persisted opt-in.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How a routed concept's context reaches the prompt once injection exists
/// (plan §6.3). Carried in the config from R0 so the GUI surface and the
/// on-disk shape are stable before R2 wires injection; **inert until R2**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionMode {
    /// Additive, safe, strictly more context than today: the concept slot
    /// rides *alongside* the existing RAG slot. The default.
    #[default]
    Augment,
    /// Experimental: routed concept snippets take the RAG slot; raw RAG runs
    /// only when routing returned nothing. Gated on the R4 eval.
    Replace,
}

/// The master routing config. Every field has a privacy/behaviour-safe
/// default, and the whole struct round-trips through JSON; an older file
/// missing a key reads that key as its default (forward-compatible, like the
/// privacy-prefs and Ollama-settings stores).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// **THE MASTER TOGGLE.** `false` ⇒ the routing path is never entered and
    /// retrieval is byte-identical to today. Default `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Never inject silently (Aaron's lock): show the candidate menu before
    /// any injection. Default `true`. **Inert until R2** (no injection yet).
    #[serde(default = "default_true")]
    pub curate_before_inject: bool,

    /// Augment (default) vs Replace. **Inert until R2.**
    #[serde(default)]
    pub mode: InjectionMode,

    /// Cap on how many concepts a turn may activate. Default 3.
    #[serde(default = "default_max_concepts")]
    pub max_concepts: usize,

    /// Absolute cosine floor for activation. **Provisional 0.50** — concept
    /// centroids are *means*, so their cosines run flatter than the
    /// chunk-level `MIN_ABSOLUTE_SCORE` (0.55); R1 logs real numbers so R4 can
    /// calibrate this the way the chunk floor was (plan §6.1, §8 risk 1).
    #[serde(default = "default_abs_threshold")]
    pub abs_threshold: f32,

    /// Relative floor: a concept must also clear `relative_floor · top` to
    /// activate. Mirrors `rank_with`'s `RELATIVE_FLOOR`. Default 0.6.
    #[serde(default = "default_relative_floor")]
    pub relative_floor: f32,

    /// Narrow each activated concept's members to the active file's region
    /// (the lens). Default `true`. **Inert until R3.**
    #[serde(default = "default_true")]
    pub scope_to_active_region: bool,
}

fn default_true() -> bool {
    true
}
fn default_max_concepts() -> usize {
    3
}
fn default_abs_threshold() -> f32 {
    0.50
}
fn default_relative_floor() -> f32 {
    0.6
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            curate_before_inject: true,
            mode: InjectionMode::Augment,
            max_concepts: default_max_concepts(),
            abs_threshold: default_abs_threshold(),
            relative_floor: default_relative_floor(),
            scope_to_active_region: true,
        }
    }
}

impl RoutingConfig {
    /// Parse the on-disk shape. Tolerant: a non-object, a missing key, or a
    /// wrong-typed value all fall back to the field default (so the whole file
    /// degrading to `default()` keeps routing **off**, never silently on).
    pub fn from_value(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Serialise to the on-disk JSON shape.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

/// `<data_dir>` resolved exactly the way every other service + the shared
/// encryption store does (`WYLDE_DATA_DIR` → `DATA_DIR` →
/// `<WYLDE_ROOT>/.wylde/data`). Read on every call so tests can point the env
/// at a scratch dir per-case.
fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(v);
    }
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join(".wylde").join("data")
}

/// `<data_dir>/settings/concept_routing.json` — alongside the other settings
/// stores (`privacy.json`, `ollama.json`, `encryption_at_rest.json`).
fn config_path() -> PathBuf {
    data_dir().join("settings").join("concept_routing.json")
}

/// Read the config from a specific path. Any failure (missing file, bad JSON)
/// yields the default (routing **off**) rather than erroring — a fresh install
/// has no file, and a corrupt file must fail *closed*, never on.
fn read_from_path(path: &std::path::Path) -> RoutingConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<Value>(&s)
            .map(|v| RoutingConfig::from_value(&v))
            .unwrap_or_default(),
        Err(_) => RoutingConfig::default(),
    }
}

/// Write the config to a specific path, creating the parent dir. Writes to a
/// sibling `.tmp` then renames so a crash mid-write can't leave a half-written
/// (and thus parse-failing → fail-off) file. Mirrors the privacy-prefs writer.
fn write_to_path(path: &std::path::Path, cfg: &RoutingConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("concept_routing: mkdir: {e}"))?;
    }
    let body = serde_json::to_vec_pretty(&cfg.to_value())
        .map_err(|e| format!("concept_routing: encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("concept_routing: write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("concept_routing: rename: {e}"))?;
    Ok(())
}

/// Process-global cache, lazily seeded from disk on first access.
static CACHE: OnceLock<Mutex<RoutingConfig>> = OnceLock::new();

fn cache() -> &'static Mutex<RoutingConfig> {
    CACHE.get_or_init(|| Mutex::new(read_from_path(&config_path())))
}

impl RoutingConfig {
    /// Current snapshot — a cheap copy out of the in-memory cache (seeded from
    /// disk on first access). Safe to call on the per-turn hot path.
    pub fn current() -> RoutingConfig {
        *cache().lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist a new snapshot: update the cache **and** write it to disk. The
    /// cache is updated even when the disk write fails, so the in-session
    /// behaviour matches what the user just chose; the `Err` is handed back to
    /// surface in a banner (the optimistic-write model the Settings panel
    /// uses).
    pub fn persist(next: RoutingConfig) -> Result<(), String> {
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = next;
        write_to_path(&config_path(), &next)
    }

    /// Force-refresh the cache from disk. The facade verbs persist through
    /// [`persist`] (cache stays coherent), but a process that *wrote* the file
    /// out-of-band (or a test) can resync with this.
    pub fn reload_from_disk() -> RoutingConfig {
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
    fn default_is_off_and_safe() {
        let c = RoutingConfig::default();
        assert!(!c.enabled, "master toggle defaults OFF");
        assert!(c.curate_before_inject, "never silent by default");
        assert_eq!(c.mode, InjectionMode::Augment);
        assert_eq!(c.max_concepts, 3);
        assert!((c.abs_threshold - 0.50).abs() < 1e-6);
        assert!((c.relative_floor - 0.6).abs() < 1e-6);
        assert!(c.scope_to_active_region);
    }

    #[test]
    fn missing_keys_default_to_off_safe() {
        // An empty object (fresh install / older file) → all defaults, off.
        let c = RoutingConfig::from_value(&json!({}));
        assert!(!c.enabled);
        assert!(c.curate_before_inject);
        // A partial object only flips the key it carries.
        let c = RoutingConfig::from_value(&json!({ "enabled": true }));
        assert!(c.enabled);
        assert_eq!(c.max_concepts, 3, "unset knob keeps its default");
    }

    #[test]
    fn malformed_value_fails_closed() {
        // A wrong-typed file must read as default (off), never as on.
        let c = RoutingConfig::from_value(&json!({ "enabled": "yes", "max_concepts": "lots" }));
        assert!(!c.enabled, "garbage fails closed to off");
    }

    #[test]
    fn value_round_trips() {
        let c = RoutingConfig {
            enabled: true,
            curate_before_inject: false,
            mode: InjectionMode::Replace,
            max_concepts: 5,
            abs_threshold: 0.42,
            relative_floor: 0.7,
            scope_to_active_region: false,
        };
        assert_eq!(RoutingConfig::from_value(&c.to_value()), c);
    }

    #[test]
    fn disk_round_trip_through_path() {
        // Hermetic: write + read a scratch path directly, bypassing the
        // process-global cache (which other tests in the binary share).
        let dir = std::env::temp_dir().join(format!("wylde-cr-cfg-{}", std::process::id()));
        let path = dir.join("settings").join("concept_routing.json");
        // Missing file → default (off).
        assert_eq!(read_from_path(&path), RoutingConfig::default());
        let cfg = RoutingConfig {
            enabled: true,
            ..RoutingConfig::default()
        };
        write_to_path(&path, &cfg).expect("write");
        assert_eq!(read_from_path(&path), cfg);
        assert!(!path.with_extension("json.tmp").exists(), "no leftover tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_reads_as_default_off() {
        let dir = std::env::temp_dir().join(format!("wylde-cr-corrupt-{}", std::process::id()));
        let path = dir.join("concept_routing.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ not json ]").unwrap();
        assert_eq!(read_from_path(&path), RoutingConfig::default());
        assert!(!read_from_path(&path).enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn current_and_persist_through_env_dir() {
        // Drive the real cache + path resolution via WYLDE_DATA_DIR. Serial:
        // mutates a process-global env var + the shared cache.
        let dir = std::env::temp_dir().join(format!("wylde-cr-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("WYLDE_DATA_DIR", &dir);

        // Fresh: reload picks up the (absent) file as default-off.
        assert!(!RoutingConfig::reload_from_disk().enabled);

        // Persist on, then reload from disk proves it stuck.
        RoutingConfig::persist(RoutingConfig {
            enabled: true,
            ..RoutingConfig::default()
        })
        .expect("persist");
        assert!(RoutingConfig::current().enabled);
        assert!(RoutingConfig::reload_from_disk().enabled);

        std::env::remove_var("WYLDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        // Restore the cache to default so later tests in the binary aren't
        // left seeing `enabled = true`.
        RoutingConfig::reload_from_disk();
    }
}
