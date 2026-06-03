//! Runtime model-selection state. Rust port of
//! `Core/harness/backend/model_state.py`.
//!
//! Three concerns share this module, matching the Python file:
//!
//! 1. **Capability cache** — process-local sticky-fallback for tool
//!    support. If a model rejected the `tools` field once we stop
//!    sending it; [`forget_model`] clears that on a model swap/delete.
//! 2. **Active-model selection** — persisted to `$DATA_DIR/active_model.json`
//!    so the inference bar's current pick survives restarts and is
//!    observable across processes via the `models.set_active` pipe verb.
//! 3. **Default-model selection** — the user's *starred* preference,
//!    persisted to `$DATA_DIR/default_model.json`, with a
//!    `WYLDE_DEFAULT_MODEL` env fallback. Distinct from active: this is
//!    the "start new chats with" choice.
//!
//! ## Storage backend
//!
//! The Python module reads/writes two tiny JSON files (`{"model": "..."}`).
//! There is no shared sqlite/db layer — these are flat files in the
//! harness data dir. This Rust port talks to the same files honouring the
//! same env overrides (`ACTIVE_MODEL_PATH`, `DEFAULT_MODEL_PATH`,
//! `DATA_DIR`) so the two impls round-trip the same on-disk state. The
//! value cache mirrors Python's module-global `_cached` / `_loaded`
//! latch; [`reset_for_tests`] drops it so a test that re-points the path
//! env re-reads from disk.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// One lazily-loaded persisted selection (active or default). Mirrors
/// Python's `(_cached, _loaded)` / `(_default_cached, _default_loaded)`
/// module globals. `Mutex::new` is const here because `None`/`false`
/// are const initialisers.
struct Selection {
    cached: Option<String>,
    loaded: bool,
}

static ACTIVE: Mutex<Selection> = Mutex::new(Selection {
    cached: None,
    loaded: false,
});

static DEFAULT: Mutex<Selection> = Mutex::new(Selection {
    cached: None,
    loaded: false,
});

/// Process-local capability cache: the set of model names that have
/// rejected the `tools` field. `HashSet::new()` is not const, so the set
/// lives behind a `OnceLock`.
fn tool_failures() -> &'static Mutex<HashSet<String>> {
    static TF: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    TF.get_or_init(|| Mutex::new(HashSet::new()))
}

// ── Path resolution (Python parity) ────────────────────────────────────

/// `$DATA_DIR` (default `"data"`) — the harness data dir. Note this is a
/// *different* root from the model-registry store (`MODEL_DATA_DIR`,
/// default `data/model_registry`); the Python `model_state` module reads
/// `DATA_DIR` directly, so we match that.
fn data_dir() -> PathBuf {
    std::env::var_os("DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"))
}

fn active_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ACTIVE_MODEL_PATH") {
        return PathBuf::from(p);
    }
    data_dir().join("active_model.json")
}

fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("DEFAULT_MODEL_PATH") {
        return PathBuf::from(p);
    }
    data_dir().join("default_model.json")
}

/// Trim + treat empty as "unset". Mirrors Python's
/// `(name or "").strip() or None`.
fn clean(name: Option<&str>) -> Option<String> {
    let trimmed = name.unwrap_or("").trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Read the `{"model": "..."}` shape from `path`, returning the cleaned
/// name or `None`. Any error (missing/unreadable/malformed) → `None`,
/// matching Python's broad `except`.
fn read_disk(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = value.get("model").and_then(serde_json::Value::as_str)?;
    clean(Some(name))
}

/// Write the `{"model": name}` shape to `path` (empty string when
/// cleared), creating the parent dir. Errors are swallowed like Python's
/// `except` that only logs.
fn write_disk(path: &std::path::Path, name: Option<&str>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent); // wylde-check: discard-result-ok
    }
    let body = serde_json::json!({ "model": name.unwrap_or("") });
    let serialised = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_owned());
    let _ = std::fs::write(path, serialised); // wylde-check: discard-result-ok
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ── Capability cache ───────────────────────────────────────────────────

/// Return `false` only if we previously saw `model` reject the `tools`
/// field. Empty model → `true` (no opinion).
pub fn model_supports_tools(model: &str) -> bool {
    if model.is_empty() {
        return true;
    }
    !lock(tool_failures()).contains(model)
}

/// Record that `model` does not handle the `tools` field — strip it next
/// time. No-op for an empty name.
pub fn mark_tool_failure(model: &str) {
    if model.is_empty() {
        return;
    }
    lock(tool_failures()).insert(model.to_owned());
}

/// Drop any cached capability state for a single model (e.g. on model
/// swap or delete). No-op for an empty name.
pub fn forget_model(model: &str) {
    if model.is_empty() {
        return;
    }
    lock(tool_failures()).remove(model);
}

/// Drop the entire capability cache (test setup / full reload).
pub fn reset_capabilities() {
    lock(tool_failures()).clear();
}

// ── Active-model selection ─────────────────────────────────────────────

/// The persisted active model, or `None` if none chosen yet.
pub fn get_active_model() -> Option<String> {
    let mut cell = lock(&ACTIVE);
    if !cell.loaded {
        cell.cached = read_disk(&active_path());
        cell.loaded = true;
    }
    cell.cached.clone()
}

/// Persist `name` as the active model and clear capability state for the
/// previously-active model so a future swap back doesn't inherit stale
/// flags. Empty / `None` unsets. Returns the persisted value.
pub fn set_active_model(name: Option<&str>) -> Option<String> {
    let cleaned = clean(name);
    let previous = {
        let mut cell = lock(&ACTIVE);
        let previous = if cell.loaded {
            cell.cached.clone()
        } else {
            read_disk(&active_path())
        };
        cell.cached = cleaned.clone();
        cell.loaded = true;
        write_disk(&active_path(), cleaned.as_deref());
        previous
    };
    if let Some(prev) = previous {
        if Some(&prev) != cleaned.as_ref() {
            forget_model(&prev);
        }
    }
    cleaned
}

// ── Default-model selection ────────────────────────────────────────────

/// The user's starred default model. Resolution order: persisted choice
/// → `WYLDE_DEFAULT_MODEL` env → `None`.
pub fn get_default_model() -> Option<String> {
    {
        let mut cell = lock(&DEFAULT);
        if !cell.loaded {
            cell.cached = read_disk(&default_path());
            cell.loaded = true;
        }
        if let Some(v) = &cell.cached {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    let env = std::env::var("WYLDE_DEFAULT_MODEL").unwrap_or_default();
    let env = env.trim();
    if env.is_empty() {
        None
    } else {
        Some(env.to_owned())
    }
}

/// Persist `name` as the starred default. Empty / `None` clears it
/// (subsequent reads fall back to `WYLDE_DEFAULT_MODEL` then `None`).
/// Returns the persisted value.
pub fn set_default_model(name: Option<&str>) -> Option<String> {
    let cleaned = clean(name);
    let mut cell = lock(&DEFAULT);
    cell.cached = cleaned.clone();
    cell.loaded = true;
    write_disk(&default_path(), cleaned.as_deref());
    cleaned
}

/// Test-only: drop the cached active/default selections and the
/// capability cache so the next read re-loads from whatever path the env
/// currently points at. The on-disk files are left untouched.
pub fn reset_for_tests() {
    let mut a = lock(&ACTIVE);
    a.cached = None;
    a.loaded = false;
    drop(a);
    let mut d = lock(&DEFAULT);
    d.cached = None;
    d.loaded = false;
    drop(d);
    reset_capabilities();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use tempfile::tempdir;

    /// Point the active + default paths at a fresh tempdir and clear the
    /// caches. Returns the dir guard (keep it alive for the test).
    fn isolated() -> tempfile::TempDir {
        let td = tempdir().unwrap();
        std::env::set_var("ACTIVE_MODEL_PATH", td.path().join("active_model.json"));
        std::env::set_var("DEFAULT_MODEL_PATH", td.path().join("default_model.json"));
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        reset_for_tests();
        td
    }

    #[test]
    fn active_round_trips_and_clears() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = isolated();
        assert_eq!(get_active_model(), None);
        assert_eq!(set_active_model(Some("qwen3:0.6b")), Some("qwen3:0.6b".to_owned()));
        // Drop the cache to prove it persisted to disk.
        reset_for_tests();
        assert_eq!(get_active_model(), Some("qwen3:0.6b".to_owned()));
        assert_eq!(set_active_model(Some("   ")), None);
        reset_for_tests();
        assert_eq!(get_active_model(), None);
    }

    #[test]
    fn set_active_forgets_previous_capability_flag() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = isolated();
        set_active_model(Some("old:model"));
        mark_tool_failure("old:model");
        assert!(!model_supports_tools("old:model"));
        // Swapping away from old:model must clear its sticky flag.
        set_active_model(Some("new:model"));
        assert!(model_supports_tools("old:model"));
    }

    #[test]
    fn default_falls_back_to_env_then_none() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = isolated();
        assert_eq!(get_default_model(), None);
        std::env::set_var("WYLDE_DEFAULT_MODEL", "gemma3:4b");
        assert_eq!(get_default_model(), Some("gemma3:4b".to_owned()));
        // A persisted choice wins over the env fallback.
        set_default_model(Some("llama3.2:1b"));
        assert_eq!(get_default_model(), Some("llama3.2:1b".to_owned()));
        // Clearing falls back to the env again.
        set_default_model(None);
        assert_eq!(get_default_model(), Some("gemma3:4b".to_owned()));
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
    }

    #[test]
    fn default_persists_across_cache_reset() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = isolated();
        set_default_model(Some("phi4:latest"));
        reset_for_tests();
        assert_eq!(get_default_model(), Some("phi4:latest".to_owned()));
    }

    #[test]
    fn capability_cache_tracks_failures() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = isolated();
        assert!(model_supports_tools("m"));
        mark_tool_failure("m");
        assert!(!model_supports_tools("m"));
        forget_model("m");
        assert!(model_supports_tools("m"));
        // Empty name is a no-op / no-opinion.
        assert!(model_supports_tools(""));
        mark_tool_failure("");
        assert!(model_supports_tools(""));
    }
}
