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
//! Two tiny JSON files (`{"model": "..."}`) — there is no shared
//! sqlite/db layer. The env overrides `ACTIVE_MODEL_PATH`,
//! `DEFAULT_MODEL_PATH` and `DATA_DIR` are all still honoured; the first
//! two name a file outright, the third is convention A's second arm. The
//! value cache mirrors the original module-global `_cached` / `_loaded`
//! latch; [`reset_for_tests`] drops it so a test that re-points the path
//! env re-reads from disk.
//!
//! ## Where they live (#250)
//!
//! Canonical: `<data_dir>/active_model.json`, `<data_dir>/default_model.json`
//! where `<data_dir>` is [`wylde_shared::paths::data_dir`] — convention A,
//! `WYLDE_DATA_DIR` → `DATA_DIR` → `<WYLDE_ROOT>/.wylde/data`.
//!
//! Before #250 this module resolved `DATA_DIR` **only**, falling back to a
//! *cwd-relative* `"data"`: the store's location was a property of the
//! process working directory, stable only because lifecycle pins that to
//! `wylde_root()`. A harness started anywhere else read and wrote a
//! different set of files. The legacy location — `<WYLDE_ROOT>/data` — is
//! adopted on first touch, so an existing install's starred default is not
//! reset by the move (see [`wylde_shared::data_migration`]).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use wylde_shared::data_migration::adopt_legacy_file;
use wylde_shared::paths::{data_dir, legacy_data_dir};

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

// ── Path resolution ────────────────────────────────────────────────────

/// Filename of the active-model store, under whichever root resolves.
const ACTIVE_FILE: &str = "active_model.json";
/// Filename of the starred-default store.
const DEFAULT_FILE: &str = "default_model.json";

/// Resolve one selection file under convention A, adopting the pre-#250
/// `<WYLDE_ROOT>/data/<file>` copy first if this install still has one.
///
/// The adoption runs on every resolution rather than behind a `OnceLock`:
/// its steady-state cost is a single `exists()` stat, and a process-wide
/// latch would be wrong here because both this module's tests and the
/// #243 update-survival suite rebind the path env mid-process.
fn selection_path(file: &str) -> PathBuf {
    let canonical = data_dir().join(file);
    adopt_legacy_file(&legacy_data_dir().join(file), &canonical);
    canonical
}

fn active_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ACTIVE_MODEL_PATH") {
        return PathBuf::from(p);
    }
    selection_path(ACTIVE_FILE)
}

fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("DEFAULT_MODEL_PATH") {
        return PathBuf::from(p);
    }
    selection_path(DEFAULT_FILE)
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

/// The *persisted* starred default only — the on-disk
/// `default_model.json` value, with **no** `WYLDE_DEFAULT_MODEL` env
/// fallback. Lets [`models.get_effective`] distinguish a real saved star
/// (`source: "default"`) from a value that only came from the env
/// (`source: "env"`).
pub fn get_persisted_default() -> Option<String> {
    let mut cell = lock(&DEFAULT);
    if !cell.loaded {
        cell.cached = read_disk(&default_path());
        cell.loaded = true;
    }
    cell.cached.clone().filter(|v| !v.is_empty())
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

/// The resolved on-disk path of the starred-default store.
///
/// Exposed so the update-safety gate (#243) can assert *where* the store
/// lands, not merely that a write round-trips. The persistent default has
/// to survive an update, not just a shutdown (the guarantee #132 gave
/// installed models), and that is a property of this path's location
/// relative to the stack directory the updater replaces — so a test has to
/// be able to see it.
pub fn default_model_store_path() -> PathBuf {
    default_path()
}

/// The resolved on-disk path of the active-model store. Same rationale as
/// [`default_model_store_path`] — the inference bar's pick is selection
/// state with the same update-survival requirement.
pub fn active_model_store_path() -> PathBuf {
    active_path()
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

    /// Restores every path-affecting env var this module reads on drop, so a
    /// test that rebinds the *root* (rather than the two file overrides)
    /// cannot leak into the next test or into the ambient dev install.
    struct EnvSandbox {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvSandbox {
        /// Clear every override and pin `WYLDE_ROOT` at `root`, so path
        /// resolution goes through the convention-A fallback — the arm #250
        /// changed, and the only one a legacy-adoption test can exercise.
        fn rooted_at(root: &std::path::Path) -> Self {
            const VARS: [&str; 5] = [
                "WYLDE_ROOT",
                "WYLDE_DATA_DIR",
                "DATA_DIR",
                "ACTIVE_MODEL_PATH",
                "DEFAULT_MODEL_PATH",
            ];
            let saved = VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            std::env::set_var("WYLDE_ROOT", root);
            std::env::remove_var("WYLDE_DEFAULT_MODEL");
            reset_for_tests();
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
            reset_for_tests();
        }
    }

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
        assert_eq!(
            set_active_model(Some("qwen3:0.6b")),
            Some("qwen3:0.6b".to_owned())
        );
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

    // ── #250: convention A, and no data lost getting there ──────────────

    /// The store roots at `<WYLDE_ROOT>/.wylde/data`, not at the process
    /// working directory. Pre-#250 this fell back to a bare relative
    /// `"data"`, so a harness launched from anywhere but the estate root
    /// silently used a different set of files (the H6 hazard `paths.rs`
    /// documents).
    #[test]
    fn selection_stores_root_under_convention_a_not_the_cwd() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());

        let expected = td.path().join(".wylde").join("data");
        for (label, path) in [
            ("default", default_model_store_path()),
            ("active", active_model_store_path()),
        ] {
            assert!(
                path.starts_with(&expected),
                "{label}-model store resolved to {} — convention A is {}",
                path.display(),
                expected.display()
            );
            assert!(
                path.is_absolute(),
                "{label}-model store is cwd-relative: {}",
                path.display()
            );
        }
    }

    /// THE upgrade guarantee: data present only at the legacy path, nothing
    /// at the canonical one, still reads correctly after the move. Without
    /// the adoption this returns `None` and the user's starred model is
    /// silently gone, with no error anywhere.
    #[test]
    fn a_legacy_only_default_is_still_read_after_the_move() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());

        // The pre-#250 layout on a live install: `<ROOT>/data/*.json`.
        let legacy = td.path().join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("default_model.json"),
            br#"{"model":"gemma3:4b"}"#,
        )
        .unwrap();
        std::fs::write(
            legacy.join("active_model.json"),
            br#"{"model":"phi4:latest"}"#,
        )
        .unwrap();
        assert!(
            !td.path().join(".wylde").join("data").exists(),
            "precondition: nothing at the canonical root yet"
        );

        assert_eq!(get_default_model(), Some("gemma3:4b".to_owned()));
        assert_eq!(get_active_model(), Some("phi4:latest".to_owned()));

        // The bytes really moved — a later read does not depend on the
        // legacy file still being there.
        let canonical = td.path().join(".wylde").join("data");
        assert!(canonical.join("default_model.json").is_file());
        assert!(canonical.join("active_model.json").is_file());
        // ...and the legacy copy is preserved, so a downgrade still reads it.
        assert!(legacy.join("default_model.json").is_file());
    }

    /// One-way and idempotent: a value written since the move outranks the
    /// legacy one, and re-running adoption never resurrects the stale value.
    #[test]
    fn a_stale_legacy_value_never_overwrites_the_canonical_one() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());

        let legacy = td.path().join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("default_model.json"), br#"{"model":"old:1b"}"#).unwrap();

        // The user re-stars after upgrading.
        set_default_model(Some("new:9b"));
        // Every subsequent resolution re-runs adoption; none may clobber.
        for _ in 0..3 {
            reset_for_tests();
            assert_eq!(
                get_default_model(),
                Some("new:9b".to_owned()),
                "the canonical value is newer by construction"
            );
        }
        // The legacy file is left exactly as it was — never written to.
        assert_eq!(
            std::fs::read_to_string(legacy.join("default_model.json")).unwrap(),
            r#"{"model":"old:1b"}"#
        );
    }

    /// The explicit file overrides are a test seam and an operator escape
    /// hatch; they must still win outright, with no adoption behind them.
    #[test]
    fn explicit_path_overrides_still_win_over_convention_a() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let _env = EnvSandbox::rooted_at(td.path());

        let elsewhere = td.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::env::set_var("DEFAULT_MODEL_PATH", elsewhere.join("d.json"));
        std::env::set_var("ACTIVE_MODEL_PATH", elsewhere.join("a.json"));
        reset_for_tests();

        assert_eq!(default_model_store_path(), elsewhere.join("d.json"));
        assert_eq!(active_model_store_path(), elsewhere.join("a.json"));

        // `DATA_DIR` — convention A's second arm — likewise still wins over
        // the `<WYLDE_ROOT>/.wylde/data` fallback.
        std::env::remove_var("DEFAULT_MODEL_PATH");
        std::env::remove_var("ACTIVE_MODEL_PATH");
        std::env::set_var("DATA_DIR", &elsewhere);
        reset_for_tests();
        assert_eq!(
            default_model_store_path(),
            elsewhere.join("default_model.json")
        );
    }
}
