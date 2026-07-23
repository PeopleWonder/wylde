//! LLM-kind capability routing & benchmarking. Rust port of
//! `Core/harness/model_registry/_routing/`.
//!
//! Shared infrastructure (data-dir resolution, JSON helpers, churn
//! constants, the process-wide lock) lives in this `mod.rs`. Each
//! sub-concern lives in its own submodule:
//!
//! * [`profiles`] — profile schema & storage (get/upsert/list).
//! * [`slots`] — `CAPABILITY_SLOTS`, `select_model`.
//! * [`churn`] — promotion / swap eligibility / pending swaps.
//! * [`hf_search`] — opt-in HuggingFace discovery + status.
//!
//! Benchmarks + ollama_watcher are NOT ported here — those Ollama-talking
//! files were absorbed into `wylde-ollama` during Phase 1 per the master
//! plan.
//!
//! ## Privacy contract
//!
//! Network discovery is OFF by default. The package NEVER makes outbound
//! HTTP calls to HuggingFace unless the user explicitly enables it via
//! `MODEL_DISCOVERY_ENABLED=true`. `MODEL_DISCOVERY_SCHEDULE=weekly` is
//! only consulted when enabled.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

pub mod churn;
pub mod hf_search;
pub mod profiles;
pub mod slots;

// ── Churn-prevention constants (mirror Python's `_routing/__init__.py`) ─

/// Candidate must beat incumbent by ≥10%.
pub(super) const MIN_DELTA_PCT: f64 = 0.10;
/// 5% bonus for models active > 30 days.
pub(super) const INCUMBENT_BONUS: f64 = 0.05;
/// Must run benchmark ≥3 times before promotion.
pub(super) const MIN_BENCHMARK_RUNS: u64 = 3;
/// Per capability slot.
pub(super) const MAX_SWAP_PER_WEEK: usize = 1;
/// Keep previous model as fallback for 14 days.
pub(super) const FALLBACK_DAYS: i64 = 14;

/// Process-wide RW lock guarding the on-disk profile / swap stores.
/// Same shape as Python's `_lock = threading.Lock()`.
pub(super) static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Subdirectory of the canonical data root holding the registry's files.
const STORE_SUBDIR: &str = "model_registry";

/// Resolve the on-disk store root for profile / swap files:
/// `MODEL_DATA_DIR` → `<data_dir>/model_registry`, where `<data_dir>` is
/// [`wylde_shared::paths::data_dir`] (convention A).
///
/// Before #250 the fallback was a **cwd-relative** `data/model_registry`,
/// honouring neither `WYLDE_DATA_DIR` nor `WYLDE_ROOT` — so which routing
/// profiles a process saw was a property of its working directory, stable
/// only because lifecycle pins that to `wylde_root()`. The pre-#250
/// location `<WYLDE_ROOT>/data/model_registry` is adopted on first touch;
/// this store holds the largest data volume of the four #250 moved, and
/// re-benchmarking from scratch is the visible cost of losing it.
///
/// Named `store_root`, not `data_dir`: the `single_data_dir_resolver` gate
/// requires that no crate but `wylde-shared/src/paths.rs` define one.
pub fn store_root() -> PathBuf {
    if let Some(v) = std::env::var_os("MODEL_DATA_DIR") {
        return PathBuf::from(v);
    }
    let canonical = wylde_shared::paths::data_dir().join(STORE_SUBDIR);
    wylde_shared::data_migration::adopt_legacy_tree(
        &wylde_shared::paths::legacy_data_dir().join(STORE_SUBDIR),
        &canonical,
    );
    canonical
}

pub(super) fn profiles_file() -> PathBuf {
    store_root().join("profiles.json")
}

pub(super) fn swaps_file() -> PathBuf {
    store_root().join("swaps.json")
}

pub(super) fn discovery_file() -> PathBuf {
    store_root().join("discovery.json")
}

pub(super) fn pending_swaps_file() -> PathBuf {
    store_root().join("pending_swaps.json")
}

/// Read JSON from `path`, returning `default` if the file is missing,
/// unreadable, or malformed. Matches Python's `_load_json` semantics
/// exactly: open errors / parse errors both fall through silently.
pub(super) fn load_json(path: &std::path::Path, default: Value) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return default;
    };
    serde_json::from_str(&text).unwrap_or(default)
}

/// Write JSON to `path`. Creates the parent directory if missing —
/// matches Python's `DATA_DIR.mkdir(parents=True, exist_ok=True)` done
/// at module load time, but on every write so tests don't need to
/// pre-create the dir.
pub(super) fn save_json(path: &std::path::Path, data: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialised = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_owned());
    std::fs::write(path, serialised)
}

/// Read `MODEL_DISCOVERY_ENABLED` env var. Default `false`. Same parsing
/// rule as Python: `("1", "true", "yes")` case-insensitive enables.
pub fn discovery_enabled() -> bool {
    let raw = std::env::var("MODEL_DISCOVERY_ENABLED").unwrap_or_default();
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// Read `MODEL_DISCOVERY_SCHEDULE`. Default `weekly`.
pub fn discovery_schedule() -> String {
    std::env::var("MODEL_DISCOVERY_SCHEDULE").unwrap_or_else(|_| "weekly".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use tempfile::tempdir;

    /// Restore every root-affecting env var on drop, and pin `WYLDE_ROOT`
    /// at `root` with no `MODEL_DATA_DIR`/`WYLDE_DATA_DIR`/`DATA_DIR` — the
    /// convention-A fallback arm, which is the one #250 changed.
    struct EnvSandbox {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvSandbox {
        fn rooted_at(root: &std::path::Path) -> Self {
            const VARS: [&str; 4] = ["WYLDE_ROOT", "WYLDE_DATA_DIR", "DATA_DIR", "MODEL_DATA_DIR"];
            let saved = VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            std::env::set_var("WYLDE_ROOT", root);
            Self { saved }
        }
    }

    impl Drop for EnvSandbox {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn store_root_honours_env_override() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());
        std::env::set_var("MODEL_DATA_DIR", td.path());
        assert_eq!(store_root(), td.path());
    }

    /// #250: the fallback is root-anchored, not cwd-relative. This test
    /// replaces one that asserted the exact opposite — that `store_root()`
    /// equalled a bare relative `data/model_registry` — which is the
    /// property the issue calls hazard 1.
    #[test]
    fn store_root_falls_back_under_convention_a_not_the_cwd() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());

        let p = store_root();
        assert_eq!(
            p,
            td.path().join(".wylde").join("data").join("model_registry")
        );
        assert!(
            p.is_absolute(),
            "a cwd-relative store root makes the profile set a property of \
             the working directory: {}",
            p.display()
        );
    }

    /// THE upgrade guarantee for the largest of the four stores: routing
    /// profiles present only under the legacy root still read after the
    /// move, and the legacy tree is preserved.
    #[test]
    fn a_legacy_only_profile_store_is_adopted_once_and_never_clobbered() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());

        // The pre-#250 layout, as a cwd-pinned launch left it.
        let legacy = td.path().join("data").join("model_registry");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("profiles.json"), r#"{"chat":{"model":"a"}}"#).unwrap();

        let profiles = profiles_file();
        assert!(
            profiles.starts_with(td.path().join(".wylde")),
            "resolved outside convention A: {}",
            profiles.display()
        );
        assert_eq!(
            load_json(&profiles, serde_json::json!(null))["chat"]["model"],
            "a",
            "an upgrade must not reset the user's routing profiles"
        );

        // One-way: the legacy tree survives for a downgrade.
        assert!(legacy.join("profiles.json").is_file());

        // Idempotent: a value written since the move outranks the legacy one,
        // however many times resolution re-runs adoption.
        save_json(&profiles, &serde_json::json!({"chat": {"model": "b"}})).unwrap();
        for _ in 0..3 {
            assert_eq!(
                load_json(&profiles_file(), serde_json::json!(null))["chat"]["model"],
                "b"
            );
        }
        assert_eq!(
            std::fs::read_to_string(legacy.join("profiles.json")).unwrap(),
            r#"{"chat":{"model":"a"}}"#,
            "the legacy tree is read-only to this migration"
        );
    }

    #[test]
    fn discovery_enabled_recognises_yes_true_one() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("MODEL_DISCOVERY_ENABLED");
        for v in ["1", "true", "TRUE", "yes", "Yes"] {
            std::env::set_var("MODEL_DISCOVERY_ENABLED", v);
            assert!(discovery_enabled(), "{v} should enable");
        }
        for v in ["0", "false", "off", ""] {
            std::env::set_var("MODEL_DISCOVERY_ENABLED", v);
            assert!(!discovery_enabled(), "{v:?} should not enable");
        }
        match prior {
            Some(v) => std::env::set_var("MODEL_DISCOVERY_ENABLED", v),
            None => std::env::remove_var("MODEL_DISCOVERY_ENABLED"),
        }
    }

    #[test]
    fn load_json_returns_default_on_missing_file() {
        let td = tempdir().unwrap();
        let p = td.path().join("missing.json");
        let v = load_json(&p, serde_json::json!({"hello": "world"}));
        assert_eq!(v["hello"], "world");
    }

    #[test]
    fn save_then_load_round_trips() {
        let td = tempdir().unwrap();
        let p = td.path().join("sub/file.json");
        let body = serde_json::json!({"a": 1, "b": "two"});
        save_json(&p, &body).unwrap();
        let got = load_json(&p, serde_json::json!({}));
        assert_eq!(got, body);
    }
}
