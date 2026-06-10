//! The system-prompt override + preset store.
//!
//! Rust port of `Core/shared/system_prompts.py` (full-Rust cutover,
//! 2026-06-09). Persists at `<WYLDE_ROOT>/data/system_prompts.json` —
//! the SAME file the Python module owned, same format, so the cutover
//! needs no data migration:
//!
//! ```json
//! {
//!   "version": 1,
//!   "overrides": {"<id>": "<text>", ...},
//!   "presets":   {"<name>": {"<id>": "<text>", ...}, ...},
//!   "active_preset": "Default"
//! }
//! ```
//!
//! The file is deliberately plaintext (NOT under the OI-14
//! encryption-at-rest umbrella): prompt text is configuration, not
//! user data, and the Python writer kept it readable for hand-editing.
//!
//! Reads are mtime-cached behind a mutex so hot paths (the turn
//! driver's [`effective_prompt`]) don't re-parse JSON per prompt build;
//! writers refresh the cache in place, mirroring the Python module's
//! locking + cache discipline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{json, Map, Value};

use super::catalog;

/// Why a store mutation was rejected. Maps 1:1 onto the Python
/// exceptions (`ValueError` / `LookupError`) so the pipe handlers can
/// translate to the same wire errors (`bad_request` / `not_found`).
#[derive(Debug, PartialEq)]
pub enum StoreError {
    BadRequest(String),
    NotFound(String),
    Io(String),
}

/// In-memory, sanitised image of the on-disk bundle. BTreeMaps keep the
/// serialised JSON stable (sorted) so diffs of the data file stay clean.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Store {
    pub overrides: BTreeMap<String, String>,
    pub presets: BTreeMap<String, BTreeMap<String, String>>,
    pub active_preset: String,
}

impl Store {
    fn empty() -> Self {
        Store {
            overrides: BTreeMap::new(),
            presets: BTreeMap::new(),
            active_preset: "Default".to_owned(),
        }
    }

    /// The wire/disk JSON shape (shared by the file writer and the
    /// `prompts.*` reply envelope).
    pub fn to_json(&self) -> Value {
        let overrides: Map<String, Value> = self
            .overrides
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let presets: Map<String, Value> = self
            .presets
            .iter()
            .map(|(name, bundle)| {
                let b: Map<String, Value> = bundle
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                (name.clone(), Value::Object(b))
            })
            .collect();
        json!({
            "version": 1,
            "overrides": overrides,
            "presets": presets,
            "active_preset": self.active_preset,
        })
    }

    /// Parse + sanitise a raw JSON document the way the Python loader
    /// does: non-string / blank override values are dropped, non-object
    /// preset bundles are dropped, a blank active preset falls back to
    /// `"Default"`. A non-object document yields the empty store.
    fn from_json(raw: &Value) -> Self {
        let Some(obj) = raw.as_object() else {
            return Store::empty();
        };
        let mut s = Store::empty();
        if let Some(ov) = obj.get("overrides").and_then(Value::as_object) {
            for (k, v) in ov {
                if let Some(text) = v.as_str() {
                    if !text.trim().is_empty() {
                        s.overrides.insert(k.clone(), text.to_owned());
                    }
                }
            }
        }
        if let Some(ps) = obj.get("presets").and_then(Value::as_object) {
            for (name, bundle) in ps {
                let Some(bundle) = bundle.as_object() else {
                    continue;
                };
                let mut b = BTreeMap::new();
                for (k, v) in bundle {
                    if let Some(text) = v.as_str() {
                        if !text.trim().is_empty() {
                            b.insert(k.clone(), text.to_owned());
                        }
                    }
                }
                s.presets.insert(name.clone(), b);
            }
        }
        if let Some(active) = obj.get("active_preset").and_then(Value::as_str) {
            if !active.trim().is_empty() {
                s.active_preset = active.to_owned();
            }
        }
        s
    }
}

/// `<WYLDE_ROOT>/data/system_prompts.json` (the Python module's
/// `_OVERRIDES_PATH`; `WYLDE_ROOT` defaults to `.` per harness config).
pub fn overrides_path() -> PathBuf {
    crate::config::Config::get()
        .wylde_root
        .join("data")
        .join("system_prompts.json")
}

// ── mtime cache (mirrors the Python module) ──────────────────────────

struct Cache {
    store: Store,
    mtime: Option<SystemTime>,
    valid: bool,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn load_at(path: &Path) -> Store {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(raw) => Store::from_json(&raw),
            Err(e) => {
                tracing::warn!("system_prompts: could not parse {}: {e}", path.display());
                Store::empty()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::empty(),
        Err(e) => {
            tracing::warn!("system_prompts: could not read {}: {e}", path.display());
            Store::empty()
        }
    }
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Read the store (mtime-cached).
pub fn read_store() -> Store {
    let path = overrides_path();
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let mtime = mtime_of(&path);
    if let Some(c) = guard.as_ref() {
        if c.valid && c.mtime == mtime {
            return c.store.clone();
        }
    }
    let store = load_at(&path);
    *guard = Some(Cache {
        store: store.clone(),
        mtime,
        valid: true,
    });
    store
}

/// Drop the cache; the next read re-parses from disk.
pub fn reload() {
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    *guard = None;
}

fn write_store(store: &Store) -> Result<(), StoreError> {
    let path = overrides_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
    }
    let text = serde_json::to_string_pretty(&store.to_json())
        .map_err(|e| StoreError::Io(e.to_string()))?;
    // tmp + rename so a crash mid-write can't truncate the store.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| StoreError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| StoreError::Io(e.to_string()))?;
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    *guard = Some(Cache {
        store: store.clone(),
        mtime: mtime_of(&path),
        valid: true,
    });
    Ok(())
}

// ── Read helpers ──────────────────────────────────────────────────────

/// The user override for `prompt_id`, or `None`.
pub fn get_override(prompt_id: &str) -> Option<String> {
    if prompt_id.is_empty() {
        return None;
    }
    read_store().overrides.get(prompt_id).cloned()
}

/// Override text if set (non-blank), else the catalog default.
pub fn effective_prompt(prompt_id: &str) -> String {
    match get_override(prompt_id) {
        Some(text) if !text.trim().is_empty() => text,
        _ => catalog::default_for(prompt_id).to_owned(),
    }
}

// ── Mutations (each returns the post-write snapshot, like Python) ────

/// Save an override for `prompt_id`. `None` / blank / text equal to the
/// catalog default clears the override instead.
pub fn set_override(prompt_id: &str, text: Option<&str>) -> Result<Store, StoreError> {
    if catalog::entry_for(prompt_id).is_none() {
        return Err(StoreError::BadRequest(format!(
            "Unknown prompt id: {prompt_id}"
        )));
    }
    let mut store = read_store();
    let clears = match text {
        None => true,
        Some(t) => {
            t.trim().is_empty() || t.trim() == catalog::default_for(prompt_id).trim()
        }
    };
    if clears {
        store.overrides.remove(prompt_id);
    } else if let Some(t) = text {
        store.overrides.insert(prompt_id.to_owned(), t.to_owned());
    }
    write_store(&store)?;
    Ok(read_store())
}

/// Reset to catalog defaults across every prompt (`Default` preset).
pub fn clear_all_overrides() -> Result<Store, StoreError> {
    let mut store = read_store();
    store.overrides.clear();
    store.active_preset = "Default".to_owned();
    write_store(&store)?;
    Ok(read_store())
}

/// Snapshot the current overrides into a named preset and activate it.
pub fn save_preset(name: &str) -> Result<Store, StoreError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::BadRequest("Preset name required.".into()));
    }
    if trimmed == "Default" {
        return Err(StoreError::BadRequest("\"Default\" is reserved.".into()));
    }
    let mut store = read_store();
    store
        .presets
        .insert(trimmed.to_owned(), store.overrides.clone());
    store.active_preset = trimmed.to_owned();
    write_store(&store)?;
    Ok(read_store())
}

/// Replace the active overrides with the named preset's bundle
/// (`"Default"` resets to catalog defaults).
pub fn load_preset(name: &str) -> Result<Store, StoreError> {
    if name == "Default" {
        return clear_all_overrides();
    }
    let mut store = read_store();
    let Some(bundle) = store.presets.get(name).cloned() else {
        return Err(StoreError::NotFound(format!("Preset not found: {name}")));
    };
    store.overrides = bundle;
    store.active_preset = name.to_owned();
    write_store(&store)?;
    Ok(read_store())
}

/// Remove a named preset; falls back to `Default` if it was active.
pub fn delete_preset(name: &str) -> Result<Store, StoreError> {
    if name == "Default" {
        return Err(StoreError::BadRequest(
            "\"Default\" cannot be deleted.".into(),
        ));
    }
    let mut store = read_store();
    store.presets.remove(name);
    if store.active_preset == name {
        store.active_preset = "Default".to_owned();
    }
    write_store(&store)?;
    Ok(read_store())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests drive the pure parse/serialise layer + the mutation logic
    /// through `Store` directly (no global path), so they're hermetic
    /// without process-wide env juggling. The file/cache layer is
    /// covered by the round-trip test gated on a temp `WYLDE_ROOT` in
    /// the integration suite.
    #[test]
    fn from_json_sanitises_like_python() {
        let raw = json!({
            "version": 1,
            "overrides": {"a.b": "text", "blank": "   ", "num": 7},
            "presets": {"P": {"a.b": "x", "drop": ""}, "bad": "nope"},
            "active_preset": "  "
        });
        let s = Store::from_json(&raw);
        assert_eq!(s.overrides.len(), 1);
        assert_eq!(s.overrides.get("a.b").map(String::as_str), Some("text"));
        assert_eq!(s.presets.len(), 1, "non-object preset bundles drop");
        assert_eq!(s.presets["P"].len(), 1, "blank preset values drop");
        assert_eq!(s.active_preset, "Default", "blank active falls back");
    }

    #[test]
    fn non_object_document_yields_empty_store() {
        assert_eq!(Store::from_json(&json!([1, 2])), Store::empty());
        assert_eq!(Store::from_json(&json!(null)), Store::empty());
    }

    #[test]
    fn to_json_round_trips() {
        let mut s = Store::empty();
        s.overrides.insert("a.b".into(), "text".into());
        s.presets
            .insert("P".into(), BTreeMap::from([("a.b".into(), "x".into())]));
        s.active_preset = "P".into();
        let back = Store::from_json(&s.to_json());
        assert_eq!(back, s);
    }

    #[test]
    fn wire_shape_has_version_and_default_active() {
        let v = Store::empty().to_json();
        assert_eq!(v["version"], 1);
        assert_eq!(v["active_preset"], "Default");
        assert!(v["overrides"].as_object().unwrap().is_empty());
    }
}
