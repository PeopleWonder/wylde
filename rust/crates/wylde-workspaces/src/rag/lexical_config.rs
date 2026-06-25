//! The lexical-retrieval master toggle + RRF-fusion knobs — `LexicalConfig`
//! (lexical-bm25 plan L0/L8).
//!
//! **Service-owned, read in-process.** The *consumer* is this crate's RAG
//! retriever ([`crate::rag::indexer::search::query_with_vec`]); the *writer* is
//! the GUI through the `settings.lexical.{get,set}` facade verbs on this
//! service's own pipe. One store, read in-process by the search hot path — not a
//! second source of truth — so we avoid the TCP↔pipe drift trap (memory
//! `wylde-settings-ollama-defaults-ux-scope`). This is the exact shape of the
//! harness-owned `RoutingConfig` (`wylde-concept-routing/src/config.rs`),
//! relocated to where *its* consumer lives.
//!
//! **Fail-safe direction is OFF.** A missing file, a corrupt file, or a
//! malformed value all resolve to [`LexicalConfig::default`], whose
//! [`LexicalConfig::enabled`] is `false` — i.e. today's exact dense-only RAG
//! behaviour. The lexical arm + RRF fusion can only ever be *added* by an
//! explicit, persisted opt-in. With the toggle OFF the tantivy index is never
//! built, never queried, and `query_with_vec` is byte-for-byte today's path.
//!
//! The cache + persistence shape is a faithful clone of `RoutingConfig`: a
//! process-global `OnceLock<Mutex<_>>` lazily seeded from disk, a `current()`
//! snapshot read, and an optimistic `persist()` that updates the cache even when
//! the disk write fails. The on-disk file is `<data_dir>/settings/lexical.json`,
//! alongside `concept_routing.json` and the other settings stores.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The master lexical/RRF config. Every field has a behaviour-safe default, and
/// the whole struct round-trips through JSON; an older file missing a key reads
/// that key as its default (forward-compatible, like `RoutingConfig` and the
/// other settings stores).
///
/// L8 live calibration (2026-06-25, `tests/lexical_eval.rs` against a real
/// 2115-chunk `nomic-embed-text` index of the Wylde `rust/` tree): the RRF
/// fusion knobs landed at `rrf_k = 60`, `w_dense = 1.0`, `w_lex = 1.0` — the
/// sweep confirmed these maximise lexical-class recall (1.000) and nDCG (0.728)
/// while holding the semantic guardrail flat (dense 0.600 → fused 0.600);
/// up-weighting dense to 1.5 collapsed lexical recall to 0.625, and lowering
/// `rrf_k` only hurt nDCG. `fused_relative_floor` calibrated from the
/// provisional 0.6 down to **0.5**, which lifted lexical-inject 75% → 88% at no
/// cost to the semantic-kept count (10.0) or off-topic silence (0%). The
/// `min_bm25` exact-token gate and the active-file focus boosts were NOT in this
/// sweep and remain **provisional**. All knobs only ever bite when `enabled` is
/// `true`. See `outputs/lexical-bm25-eval-results.md` for the full tables.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LexicalConfig {
    /// **THE MASTER TOGGLE.** `false` ⇒ the lexical arm is never built or
    /// queried and retrieval is byte-identical to today's dense-only path.
    /// Default `false`.
    #[serde(default)]
    pub enabled: bool,

    /// RRF rank-bias constant `k` in `w / (k + rank)`. The canonical Cormack et
    /// al. value is 60; larger flattens the contribution of top ranks, smaller
    /// sharpens it. Default `60.0`. **L7-tunable.**
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f64,

    /// Weight on the dense (cosine) arm's RRF contribution. Default `1.0`
    /// (symmetric with the lexical arm). **L7-tunable.**
    #[serde(default = "default_arm_weight")]
    pub w_dense: f64,

    /// Weight on the lexical (BM25) arm's RRF contribution. Default `1.0`
    /// (symmetric with the dense arm). **L7-tunable.**
    #[serde(default = "default_arm_weight")]
    pub w_lex: f64,

    /// Minimum BM25 score for a lexical hit to count as **on-topic** in the
    /// fused dynamic-k gate (§1.4). A candidate surfaced *purely* by a strong
    /// exact-token BM25 hit (low cosine) bypasses the absolute cosine floor only
    /// when its BM25 clears this — so off-topic-to-both queries still inject
    /// nothing while a genuine rare-identifier hit is admitted (the recall win).
    /// Provisional; **L7-tunable** (the sweep lands the real value). Default
    /// `1.0`.
    #[serde(default = "default_min_bm25")]
    pub min_bm25: f64,

    /// Relative dominance floor on the **fused** score (§1.4): keep the
    /// fused-sorted prefix while `fused ≥ floor · top_fused`. RRF scores are
    /// scale-free so this ratio transfers cleanly from the dense
    /// `RELATIVE_FLOOR`. **Live-calibrated to `0.5`** (L8, 2026-06-25): on the
    /// real index this kept 88% of lexical-class injections vs 75% at 0.6, with
    /// no loss to the semantic guardrail or off-topic silence.
    #[serde(default = "default_fused_relative_floor")]
    pub fused_relative_floor: f64,

    /// Active-file focus boost added to the **fused** score for a chunk from the
    /// exact file open in the editor (§3 — a focus signal, kept additive and
    /// post-RRF). Expressed at the RRF scale (a single arm's top contribution is
    /// `1/rrf_k ≈ 0.0167`), so it nudges the open file up a few ranks without
    /// dwarfing a chunk that genuinely ranks high in both arms. Default
    /// `0.0083` (≈ half one RRF arm at `rrf_k = 60`). **L8-tunable.**
    #[serde(default = "default_active_file_focus_boost")]
    pub active_file_focus_boost: f64,

    /// Active-file focus boost for a chunk that merely **shares the directory**
    /// of the open file — a weaker "same area" signal, smaller than the exact
    /// boost. Default `0.0033` (≈ a fifth of one RRF arm). **L8-tunable.**
    #[serde(default = "default_active_file_dir_focus_boost")]
    pub active_file_dir_focus_boost: f64,
}

fn default_rrf_k() -> f64 {
    60.0
}
fn default_arm_weight() -> f64 {
    1.0
}
fn default_min_bm25() -> f64 {
    // Provisional — admits genuine rare-identifier hits while rejecting
    // incidental common-word matches. The L7 sweep lands the calibrated value.
    1.0
}
fn default_fused_relative_floor() -> f64 {
    // L8 live-calibrated (2026-06-25): 0.5 kept 88% of lexical injections vs 75%
    // at the provisional 0.6, with no semantic-guardrail or off-topic-silence
    // cost. See outputs/lexical-bm25-eval-results.md.
    0.5
}
fn default_active_file_focus_boost() -> f64 {
    // ≈ 0.5 / 60 — half of one RRF arm's top contribution at the default rrf_k.
    0.5 / 60.0
}
fn default_active_file_dir_focus_boost() -> f64 {
    // ≈ 0.2 / 60 — a fifth of one RRF arm's top contribution.
    0.2 / 60.0
}

impl Default for LexicalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rrf_k: default_rrf_k(),
            w_dense: default_arm_weight(),
            w_lex: default_arm_weight(),
            min_bm25: default_min_bm25(),
            fused_relative_floor: default_fused_relative_floor(),
            active_file_focus_boost: default_active_file_focus_boost(),
            active_file_dir_focus_boost: default_active_file_dir_focus_boost(),
        }
    }
}

impl LexicalConfig {
    /// Parse the on-disk shape. Tolerant: a non-object, a missing key, or a
    /// wrong-typed value all fall back to the field default (so the whole file
    /// degrading to `default()` keeps the lexical arm **off**, never silently
    /// on).
    pub fn from_value(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Serialise to the on-disk JSON shape.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

/// `<data_dir>/settings/lexical.json` — alongside the other settings stores
/// (`concept_routing.json`, `privacy.json`, `ollama.json`). Resolved through
/// the same [`crate::common::data_dir`] every workspace store uses, read on
/// every call so tests can point the env at a scratch dir per-case.
fn config_path() -> PathBuf {
    crate::common::data_dir()
        .join("settings")
        .join("lexical.json")
}

/// Read the config from a specific path. Any failure (missing file, bad JSON)
/// yields the default (lexical **off**) rather than erroring — a fresh install
/// has no file, and a corrupt file must fail *closed*, never on.
fn read_from_path(path: &std::path::Path) -> LexicalConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<Value>(&s)
            .map(|v| LexicalConfig::from_value(&v))
            .unwrap_or_default(),
        Err(_) => LexicalConfig::default(),
    }
}

/// Write the config to a specific path, creating the parent dir. Writes to a
/// sibling `.tmp` then renames so a crash mid-write can't leave a half-written
/// (and thus parse-failing → fail-off) file. Mirrors the `RoutingConfig` writer.
fn write_to_path(path: &std::path::Path, cfg: &LexicalConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("lexical: mkdir: {e}"))?;
    }
    let body = serde_json::to_vec_pretty(&cfg.to_value())
        .map_err(|e| format!("lexical: encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("lexical: write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("lexical: rename: {e}"))?;
    Ok(())
}

/// Process-global cache, lazily seeded from disk on first access.
static CACHE: OnceLock<Mutex<LexicalConfig>> = OnceLock::new();

fn cache() -> &'static Mutex<LexicalConfig> {
    CACHE.get_or_init(|| Mutex::new(read_from_path(&config_path())))
}

impl LexicalConfig {
    /// Current snapshot — a cheap copy out of the in-memory cache (seeded from
    /// disk on first access). Safe to call on the per-turn retrieval hot path.
    pub fn current() -> LexicalConfig {
        *cache().lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist a new snapshot: update the cache **and** write it to disk. The
    /// cache is updated even when the disk write fails, so the in-session
    /// behaviour matches what the user just chose; the `Err` is handed back to
    /// surface in a banner (the optimistic-write model the Settings panel uses).
    pub fn persist(next: LexicalConfig) -> Result<(), String> {
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = next;
        write_to_path(&config_path(), &next)
    }

    /// Force-refresh the cache from disk. The facade verbs persist through
    /// [`persist`](LexicalConfig::persist) (cache stays coherent), but a test —
    /// or a process that wrote the file out-of-band — can resync with this.
    pub fn reload_from_disk() -> LexicalConfig {
        let fresh = read_from_path(&config_path());
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::TEST_ENV_LOCK;

    #[test]
    fn default_is_off_and_safe() {
        let c = LexicalConfig::default();
        assert!(!c.enabled, "master toggle defaults OFF");
        assert!((c.rrf_k - 60.0).abs() < 1e-9, "canonical RRF k");
        assert!((c.w_dense - 1.0).abs() < 1e-9, "symmetric default weights");
        assert!((c.w_lex - 1.0).abs() < 1e-9);
        assert!((c.min_bm25 - 1.0).abs() < 1e-9);
        assert!((c.fused_relative_floor - 0.5).abs() < 1e-9, "L8 live-calibrated floor");
        assert!(c.active_file_focus_boost > c.active_file_dir_focus_boost);
    }

    #[test]
    fn missing_keys_default_to_off_safe() {
        // An empty object (fresh install / older file) → all defaults, off.
        let c = LexicalConfig::from_value(&json!({}));
        assert!(!c.enabled);
        assert!((c.rrf_k - 60.0).abs() < 1e-9);
        // A partial object only flips the key it carries; knobs keep defaults.
        let c = LexicalConfig::from_value(&json!({ "enabled": true }));
        assert!(c.enabled);
        assert!((c.rrf_k - 60.0).abs() < 1e-9, "unset knob keeps its default");
    }

    #[test]
    fn malformed_value_fails_closed() {
        // A wrong-typed file must read as default (off), never as on.
        let c = LexicalConfig::from_value(&json!({ "enabled": "yes", "rrf_k": "lots" }));
        assert!(!c.enabled, "garbage fails closed to off");
    }

    #[test]
    fn value_round_trips() {
        let c = LexicalConfig {
            enabled: true,
            rrf_k: 42.0,
            w_dense: 1.5,
            w_lex: 0.8,
            min_bm25: 2.5,
            fused_relative_floor: 0.7,
            active_file_focus_boost: 0.01,
            active_file_dir_focus_boost: 0.004,
        };
        assert_eq!(LexicalConfig::from_value(&c.to_value()), c);
    }

    #[test]
    fn disk_round_trip_through_path() {
        // Hermetic: write + read a scratch path directly, bypassing the
        // process-global cache (which other tests in the binary share).
        let dir = std::env::temp_dir().join(format!("wylde-lx-cfg-{}", std::process::id()));
        let path = dir.join("settings").join("lexical.json");
        // Missing file → default (off).
        assert_eq!(read_from_path(&path), LexicalConfig::default());
        let cfg = LexicalConfig {
            enabled: true,
            ..LexicalConfig::default()
        };
        write_to_path(&path, &cfg).expect("write");
        assert_eq!(read_from_path(&path), cfg);
        assert!(!path.with_extension("json.tmp").exists(), "no leftover tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_reads_as_default_off() {
        let dir = std::env::temp_dir().join(format!("wylde-lx-corrupt-{}", std::process::id()));
        let path = dir.join("lexical.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ not json ]").unwrap();
        assert_eq!(read_from_path(&path), LexicalConfig::default());
        assert!(!read_from_path(&path).enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_and_persist_through_env_dir() {
        // Drive the real cache + path resolution via WYLDE_DATA_DIR. Locks the
        // shared env mutex: mutates a process-global env var + the shared cache.
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("wylde-lx-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("WYLDE_DATA_DIR", &dir);

        // Fresh: reload picks up the (absent) file as default-off.
        assert!(!LexicalConfig::reload_from_disk().enabled);

        // Persist on, then reload from disk proves it stuck.
        LexicalConfig::persist(LexicalConfig {
            enabled: true,
            ..LexicalConfig::default()
        })
        .expect("persist");
        assert!(LexicalConfig::current().enabled);
        assert!(LexicalConfig::reload_from_disk().enabled);

        std::env::remove_var("WYLDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        // Restore the cache to default so later tests in the binary aren't left
        // seeing `enabled = true`.
        LexicalConfig::reload_from_disk();
    }
}
