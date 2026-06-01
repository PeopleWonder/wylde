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

/// Resolve the on-disk store root for profile / swap files.
/// Honours `MODEL_DATA_DIR` (Python parity) → defaults to
/// `data/model_registry` relative to cwd. Mirrors Python's
/// `DATA_DIR = Path(os.getenv("MODEL_DATA_DIR", "data/model_registry"))`.
pub fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("MODEL_DATA_DIR") {
        return PathBuf::from(v);
    }
    PathBuf::from("data").join("model_registry")
}

pub(super) fn profiles_file() -> PathBuf {
    data_dir().join("profiles.json")
}

pub(super) fn swaps_file() -> PathBuf {
    data_dir().join("swaps.json")
}

pub(super) fn discovery_file() -> PathBuf {
    data_dir().join("discovery.json")
}

pub(super) fn pending_swaps_file() -> PathBuf {
    data_dir().join("pending_swaps.json")
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
    let serialised = serde_json::to_string_pretty(data)
        .unwrap_or_else(|_| "{}".to_owned());
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

    #[test]
    fn data_dir_honours_env_override() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let prior = std::env::var_os("MODEL_DATA_DIR");
        std::env::set_var("MODEL_DATA_DIR", td.path());
        assert_eq!(data_dir(), td.path());
        match prior {
            Some(v) => std::env::set_var("MODEL_DATA_DIR", v),
            None => std::env::remove_var("MODEL_DATA_DIR"),
        }
    }

    #[test]
    fn data_dir_falls_back_to_relative_default_when_unset() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("MODEL_DATA_DIR");
        std::env::remove_var("MODEL_DATA_DIR");
        let p = data_dir();
        assert_eq!(p, PathBuf::from("data").join("model_registry"));
        if let Some(v) = prior {
            std::env::set_var("MODEL_DATA_DIR", v);
        }
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
