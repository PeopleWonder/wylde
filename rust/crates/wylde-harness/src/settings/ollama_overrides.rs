//! Per-model Ollama inference override store.
//!
//! ## On-disk layout
//!
//! ```text
//! <data_dir>/settings/ollama/
//!   index.json                       # safe-name -> real model tag
//!   default/                         # profile_id "default"
//!     llama3.2_3b.json               # sparse overrides for "llama3.2:3b"
//!     qwen2.5_0.5b.json
//!     _global_migrated.json          # imported old flat ollama.json (if any)
//! ```
//!
//! The `<profile_id>` directory layer (default `"default"`) is the seam
//! for future **model profiles**: a profile is just another directory
//! alongside `default/`, so adding profiles later is a UI/verb change,
//! not a storage migration.
//!
//! Each `<model_safe>.json` is a **sparse** map — only the keys the user
//! has explicitly overridden. A missing file (or missing key) means "no
//! override"; the Settings panel then renders the model's own default
//! (from `ollama.get_model_defaults`) or the global fallback table as a
//! placeholder. This sparseness is what lets the panel distinguish "user
//! set temperature = 0.7" from "the model default happens to be 0.7".
//!
//! ## Filename sanitisation
//!
//! Model tags contain `:` and `/` (e.g. `llama3.2:3b`, `hf.co/repo:Q4`),
//! neither of which is a safe filename component on Windows. Both map to
//! `_`. Because that mapping is lossy (`a:b` and `a/b` collide), the
//! [`index.json`] file keeps the reverse mapping `safe -> real` so the
//! real tag is always recoverable and a collision is detectable.
//!
//! ## Migration
//!
//! On first use, if the old flat `<data_dir>/settings/ollama.json`
//! exists, its contents are copied to
//! `<data_dir>/settings/ollama/default/_global_migrated.json` (wrapped
//! with an `_migrated_from_global` marker) so no user data is lost. The
//! old file is left untouched; migration is idempotent (it no-ops once
//! the marker file exists).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The default profile id. Future model-profile work adds sibling dirs
/// (`work/`, `creative/`, …) next to `default/` under the same root.
pub const DEFAULT_PROFILE: &str = "default";

/// Filename for the imported old flat store. Prefixed with `_` so it is
/// excluded from [`list_models_with_overrides`] (it is not a real model).
const MIGRATED_GLOBAL_FILE: &str = "_global_migrated.json";

/// Resolve the on-disk data root. Precedence:
/// `WYLDE_DATA_DIR` → `DATA_DIR` → `WYLDE_ROOT/data` → `"data"`.
///
/// The `WYLDE_ROOT/data` arm matches where the Gateway wrote the old
/// flat `ollama.json`, so migration finds it on a real box; the first
/// two arms match the harness's own `model_state` / memory conventions
/// and let tests rebind the root to a temp dir.
fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(root) = std::env::var_os("WYLDE_ROOT") {
        return PathBuf::from(root).join("data");
    }
    PathBuf::from("data")
}

/// `<data_dir>/settings/ollama` — the override-store root.
fn store_root() -> PathBuf {
    data_dir().join("settings").join("ollama")
}

/// `<store_root>/index.json` — the safe-name → real-tag reverse map.
fn index_path() -> PathBuf {
    store_root().join("index.json")
}

/// `<store_root>/<profile_id>` — one profile's override directory.
fn profile_dir(profile_id: &str) -> PathBuf {
    store_root().join(sanitize(profile_id))
}

/// The old flat store the Gateway wrote: `<data_dir>/settings/ollama.json`.
fn legacy_flat_path() -> PathBuf {
    data_dir().join("settings").join("ollama.json")
}

/// Map a model tag (or profile id) to a safe filename stem: `:` and `/`
/// both become `_`. Lossy by design — the index recovers the real tag.
pub fn sanitize(model: &str) -> String {
    model.replace([':', '/'], "_")
}

/// Per-model override file: `<store_root>/<profile>/<safe>.json`.
fn override_path(profile_id: &str, model: &str) -> PathBuf {
    profile_dir(profile_id).join(format!("{}.json", sanitize(model)))
}

/// Read a JSON object from `path`, or `None` if absent/unparseable.
fn read_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// Atomically write a JSON object to `path` (tmp + rename), creating the
/// parent directory. Best-effort: errors are swallowed like the rest of
/// the harness's file stores (they only log in Python).
fn write_object(path: &Path, obj: &Map<String, Value>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent); // wylde-check: discard-result-ok
    }
    let Ok(serialized) = serde_json::to_string_pretty(&Value::Object(obj.clone())) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serialized).is_ok() {
        let _ = std::fs::rename(&tmp, path); // wylde-check: discard-result-ok
    }
}

// ── Index (safe-name → real-tag) ──────────────────────────────────────

/// Record `model` in the index under its sanitised key so the real tag
/// is recoverable from the on-disk filename.
fn index_record(model: &str) {
    let mut idx = read_object(&index_path()).unwrap_or_default();
    let key = sanitize(model);
    let already = idx.get(&key).and_then(Value::as_str) == Some(model);
    if already {
        return;
    }
    idx.insert(key, Value::String(model.to_owned()));
    write_object(&index_path(), &idx);
}

/// The current index (`safe -> real`), or empty when absent.
pub fn read_index() -> Map<String, Value> {
    read_object(&index_path()).unwrap_or_default()
}

// ── Migration ─────────────────────────────────────────────────────────

/// Copy the old flat `ollama.json` into the default profile as
/// `_global_migrated.json` (once). Idempotent: no-ops when the old file
/// is absent or the marker already exists. The old file is preserved.
pub fn migrate_legacy_if_needed() {
    let marker = profile_dir(DEFAULT_PROFILE).join(MIGRATED_GLOBAL_FILE);
    if marker.exists() {
        return;
    }
    let Some(old) = read_object(&legacy_flat_path()) else {
        return;
    };
    let mut wrapper = Map::new();
    wrapper.insert("_migrated_from_global".to_owned(), Value::Bool(true));
    wrapper.insert("values".to_owned(), Value::Object(old));
    write_object(&marker, &wrapper);
}

// ── Public read/write surface (backs the settings.ollama.* verbs) ─────

/// Sparse overrides for `model` in `profile_id`, or an empty map when
/// none are stored. Runs the one-time legacy migration first.
pub fn get_overrides(profile_id: &str, model: &str) -> Map<String, Value> {
    migrate_legacy_if_needed();
    read_object(&override_path(profile_id, model)).unwrap_or_default()
}

/// Set/merge a single override `key = value` for `model`. Returns the
/// full sparse map after the merge. Records the model in the index.
pub fn set_override(profile_id: &str, model: &str, key: &str, value: Value) -> Map<String, Value> {
    migrate_legacy_if_needed();
    let mut overrides = read_object(&override_path(profile_id, model)).unwrap_or_default();
    overrides.insert(key.to_owned(), value);
    write_object(&override_path(profile_id, model), &overrides);
    index_record(model);
    overrides
}

/// Delete a single override `key` for `model` (the ↺ reset). The field
/// then falls back to its placeholder. Returns the remaining sparse map.
/// When the last key is cleared the file is removed so the model drops
/// out of [`list_models_with_overrides`].
pub fn clear_override(profile_id: &str, model: &str, key: &str) -> Map<String, Value> {
    migrate_legacy_if_needed();
    let path = override_path(profile_id, model);
    let mut overrides = read_object(&path).unwrap_or_default();
    overrides.remove(key);
    if overrides.is_empty() {
        let _ = std::fs::remove_file(&path); // wylde-check: discard-result-ok
    } else {
        write_object(&path, &overrides);
    }
    overrides
}

/// Real model tags that have at least one stored override in
/// `profile_id`. Resolves safe filenames back to real tags via the
/// index; the `_`-prefixed migration artifact is excluded. Sorted.
pub fn list_models_with_overrides(profile_id: &str) -> Vec<String> {
    migrate_legacy_if_needed();
    let index = read_index();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(profile_dir(profile_id)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('_') {
            continue; // migration artifact, not a real model
        }
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        // Prefer the index's real tag; fall back to the safe stem if the
        // index is missing it (hand-placed file, or pre-index write).
        let real = index
            .get(stem)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| stem.to_owned());
        out.push(real);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use serde_json::json;

    /// Bind the store to a fresh temp dir for one test. Holds the shared
    /// env lock for the closure so concurrent tests don't cross-talk the
    /// `WYLDE_DATA_DIR` global.
    fn with_temp_store<F: FnOnce(&Path)>(f: F) {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let prior = std::env::var_os("WYLDE_DATA_DIR");
        // SAFETY: TEST_ENV_LOCK serialises WYLDE_DATA_DIR mutation.
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());
        f(tmp.path());
        match prior {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
    }

    #[test]
    fn sanitize_replaces_colon_and_slash() {
        assert_eq!(sanitize("llama3.2:3b"), "llama3.2_3b");
        assert_eq!(sanitize("hf.co/owner/repo:Q4"), "hf.co_owner_repo_Q4");
        assert_eq!(sanitize("plain"), "plain");
    }

    #[test]
    fn get_overrides_empty_when_none() {
        with_temp_store(|_| {
            assert!(get_overrides(DEFAULT_PROFILE, "llama3.2:3b").is_empty());
        });
    }

    #[test]
    fn set_get_clear_round_trip_is_per_model() {
        with_temp_store(|_| {
            // Two distinct models keep distinct overrides.
            set_override(DEFAULT_PROFILE, "llama3.2:3b", "temperature", json!(0.7));
            set_override(DEFAULT_PROFILE, "qwen2.5:0.5b", "temperature", json!(0.3));

            let a = get_overrides(DEFAULT_PROFILE, "llama3.2:3b");
            let b = get_overrides(DEFAULT_PROFILE, "qwen2.5:0.5b");
            assert_eq!(a.get("temperature"), Some(&json!(0.7)));
            assert_eq!(b.get("temperature"), Some(&json!(0.3)));

            // Merge a second key without clobbering the first.
            set_override(DEFAULT_PROFILE, "llama3.2:3b", "num_ctx", json!(8192));
            let a = get_overrides(DEFAULT_PROFILE, "llama3.2:3b");
            assert_eq!(a.get("temperature"), Some(&json!(0.7)));
            assert_eq!(a.get("num_ctx"), Some(&json!(8192)));

            // Clear one key → falls back (removed from the sparse map).
            let after = clear_override(DEFAULT_PROFILE, "llama3.2:3b", "temperature");
            assert!(after.get("temperature").is_none());
            assert_eq!(after.get("num_ctx"), Some(&json!(8192)));

            // Clearing the last key removes the model from the listing.
            clear_override(DEFAULT_PROFILE, "llama3.2:3b", "num_ctx");
            let listed = list_models_with_overrides(DEFAULT_PROFILE);
            assert!(!listed.contains(&"llama3.2:3b".to_owned()));
            assert!(listed.contains(&"qwen2.5:0.5b".to_owned()));
        });
    }

    #[test]
    fn index_recovers_real_tag_with_colon() {
        with_temp_store(|_| {
            set_override(DEFAULT_PROFILE, "llama3.2:3b", "seed", json!(1));
            let idx = read_index();
            assert_eq!(idx.get("llama3.2_3b"), Some(&json!("llama3.2:3b")));
            // The listing reports the REAL tag, not the safe stem.
            let listed = list_models_with_overrides(DEFAULT_PROFILE);
            assert_eq!(listed, vec!["llama3.2:3b".to_owned()]);
        });
    }

    #[test]
    fn migration_imports_old_flat_file_and_is_idempotent() {
        with_temp_store(|root| {
            // Seed the old flat store the Gateway used to write.
            let legacy = root.join("settings").join("ollama.json");
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, r#"{"temperature":0.42,"num_ctx":4096}"#).unwrap();

            // First touch migrates.
            let _ = get_overrides(DEFAULT_PROFILE, "anything:latest");
            let marker = root
                .join("settings")
                .join("ollama")
                .join("default")
                .join("_global_migrated.json");
            assert!(marker.exists(), "migration marker should be written");
            let wrapper = read_object(&marker).unwrap();
            assert_eq!(wrapper.get("_migrated_from_global"), Some(&json!(true)));
            assert_eq!(wrapper["values"]["temperature"], json!(0.42));

            // The artifact is NOT a real model in the listing.
            assert!(list_models_with_overrides(DEFAULT_PROFILE).is_empty());

            // Idempotent: rewriting legacy then re-touching doesn't clobber.
            std::fs::write(&legacy, r#"{"temperature":0.99}"#).unwrap();
            migrate_legacy_if_needed();
            let wrapper = read_object(&marker).unwrap();
            assert_eq!(wrapper["values"]["temperature"], json!(0.42));

            // The old file is preserved (never deleted).
            assert!(legacy.exists());
        });
    }

    #[test]
    fn profiles_are_isolated_by_dir() {
        with_temp_store(|_| {
            set_override(DEFAULT_PROFILE, "m:1", "temperature", json!(0.7));
            set_override("creative", "m:1", "temperature", json!(1.2));
            assert_eq!(
                get_overrides(DEFAULT_PROFILE, "m:1").get("temperature"),
                Some(&json!(0.7))
            );
            assert_eq!(
                get_overrides("creative", "m:1").get("temperature"),
                Some(&json!(1.2))
            );
        });
    }
}
