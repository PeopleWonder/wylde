//! Profile schema & storage — `get_profile`, `upsert_profile`,
//! `list_profiles`. Rust port of `_routing/profiles.py`.
//!
//! Each LLM profile is a JSON dict on disk in
//! `DATA_DIR/profiles.json`. The schema is documented inline below.
//! Mutations go through [`upsert_profile`] so the process-wide lock
//! serialises read-modify-write across threads.

use serde_json::{json, Map, Value};

use crate::model_registry::routing::{load_json, profiles_file, save_json, STORE_LOCK};

// ── Model profile schema ──────────────────────────────────────────────
//
// {
//   "name": "gemma3:27b",
//   "size_gb": 16.0,
//   "quant": "Q4_K_M",
//   "vram_footprint_mb": 14000,
//   "capabilities": ["code", "reasoning"],
//   "benchmark_scores": {
//     "tok_s_prompt": 450,
//     "tok_s_gen":    42,
//     "perplexity":   4.8,
//     "task_scores":  {"code": 0.82, "reasoning": 0.78}
//   },
//   "benchmark_runs":  3,
//   "last_benchmarked": "2025-01-01T00:00:00",
//   "first_active_at":  "2025-01-01T00:00:00",
//   "status": "active",      # active | candidate | retired | fallback
//   "slot":   "code",
//   "notes":  ""
// }

pub(super) fn read_profiles() -> Map<String, Value> {
    let v = load_json(&profiles_file(), json!({}));
    v.as_object().cloned().unwrap_or_default()
}

pub(super) fn write_profiles(profiles: &Map<String, Value>) {
    let _ = save_json(&profiles_file(), &Value::Object(profiles.clone())); // wylde-check: discard-result-ok
}

/// Look up one profile by model name. Matches Python's
/// `get_profile(name)` — returns `None` when the name is unknown.
pub fn get_profile(name: &str) -> Option<Value> {
    read_profiles().get(name).cloned()
}

/// Return every profile dict the routing layer knows about. Matches
/// Python's `list_profiles()`. Order is unspecified — callers that need
/// stability should sort by name.
pub fn list_profiles() -> Vec<Value> {
    read_profiles().into_values().collect()
}

fn default_profile(name: &str) -> Value {
    json!({
        "name": name,
        "status": "candidate",
        "benchmark_runs": 0,
        "benchmark_scores": {},
        "capabilities": [],
        "vram_footprint_mb": null,
        "size_gb": null,
        "quant": null,
        "first_active_at": null,
        "backend": "ollama",
        "backend_url": "",
        "backend_model": "",
    })
}

/// Merge `updates` into the profile for `name` under the store lock.
///
/// Matches Python's `upsert_profile(name, updates)`. `updates` is a
/// JSON object whose fields are written verbatim into the existing
/// profile (or a freshly-defaulted one). The Python callable-update
/// variant (where `updates` is a function of the existing profile) isn't
/// reachable from the Rust surface — callers compose the dict themselves.
///
/// Returns the post-merge profile.
pub fn upsert_profile(name: &str, updates: Value) -> Value {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut profiles = read_profiles();
    profiles
        .entry(name.to_owned())
        .or_insert_with(|| default_profile(name));
    if let Some(updates_map) = updates.as_object() {
        let existing = profiles.get_mut(name).expect("entry just inserted");
        let Some(existing_map) = existing.as_object_mut() else {
            // Shouldn't happen — every default profile is an object —
            // but if a hand-rolled JSON has an object→string mutation,
            // replace it wholesale rather than panicking.
            *existing = Value::Object(updates_map.clone());
            write_profiles(&profiles);
            return profiles.get(name).cloned().unwrap_or(Value::Null);
        };
        for (k, v) in updates_map {
            existing_map.insert(k.clone(), v.clone());
        }
    }
    write_profiles(&profiles);
    profiles.get(name).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::memory::common::TEST_ENV_LOCK;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// Per-test data-dir sandbox. Same shape as
    /// `crate::memory::workspaces::test_support::TestEnv` — takes the
    /// process-wide env lock, mutates `MODEL_DATA_DIR` to a tempdir,
    /// restores on drop.
    pub struct TestEnv {
        _guard: MutexGuard<'static, ()>,
        _td: TempDir,
        prior: Option<std::ffi::OsString>,
    }

    impl TestEnv {
        pub fn new() -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().expect("tempdir");
            let prior = std::env::var_os("MODEL_DATA_DIR");
            std::env::set_var("MODEL_DATA_DIR", td.path());
            Self {
                _guard: guard,
                _td: td,
                prior,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var("MODEL_DATA_DIR", v),
                None => std::env::remove_var("MODEL_DATA_DIR"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestEnv;
    use super::*;

    #[test]
    fn list_returns_empty_when_no_file() {
        let _env = TestEnv::new();
        assert!(list_profiles().is_empty());
    }

    #[test]
    fn upsert_creates_with_defaults() {
        let _env = TestEnv::new();
        let p = upsert_profile("qwen2.5:0.5b", json!({}));
        assert_eq!(p["name"], "qwen2.5:0.5b");
        assert_eq!(p["status"], "candidate");
        assert_eq!(p["benchmark_runs"], 0);
        assert_eq!(p["backend"], "ollama");
    }

    #[test]
    fn upsert_merges_into_existing_profile() {
        let _env = TestEnv::new();
        upsert_profile("a", json!({}));
        let p = upsert_profile("a", json!({"status": "active", "size_gb": 3.5}));
        assert_eq!(p["status"], "active");
        assert_eq!(p["size_gb"], 3.5);
        // Original defaults still present.
        assert_eq!(p["backend"], "ollama");
    }

    #[test]
    fn get_returns_none_for_unknown_name() {
        let _env = TestEnv::new();
        assert!(get_profile("ghost").is_none());
    }

    #[test]
    fn list_returns_every_profile() {
        let _env = TestEnv::new();
        upsert_profile("a", json!({}));
        upsert_profile("b", json!({}));
        let list = list_profiles();
        assert_eq!(list.len(), 2);
        let names: std::collections::HashSet<_> = list
            .iter()
            .map(|p| p["name"].as_str().unwrap_or("").to_owned())
            .collect();
        assert!(names.contains("a"));
        assert!(names.contains("b"));
    }
}
