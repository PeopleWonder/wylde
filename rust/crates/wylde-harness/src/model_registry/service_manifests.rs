//! Read `models` declarations from service manifests. Rust port of
//! `Core/harness/model_registry/_service_manifests.py`.
//!
//! Each top-level service folder under the Wylde root may ship a
//! `manifest.json` with a `models` array. This module collects every
//! such declaration and returns:
//!
//! * an `overrides` map (model id → kind) — wins over the heuristic.
//! * a `required_by` map (model id → list of service names) — surfaced
//!   on the entry for the GUI.
//!
//! Manifests without a `models` key are silently skipped. Tool
//! manifests under `Core/harness/tooling/tools/<group>/<id>/`
//! aren't reached because we don't scan `Core`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::model_registry::types::Kind;

/// Top-level dirs to scan. Skipping `Core` (no service manifests there)
/// and `_legacy`. Anything new added to the Wylde root that ships a
/// service manifest needs to be added here.
const SERVICE_ROOTS: &[&str] = &[
    "Voice",
    "VoiceAssistant", // wylde-check: dead-ref-ok
    "Trainer",
    "Extensions",
    "Gateway",
    "device_gate",
    "N8N",
];

type Signature = Vec<(PathBuf, u64, u64)>;

struct Cache {
    signature: Option<Signature>,
    overrides: HashMap<String, Kind>,
    required_by: HashMap<String, Vec<String>>,
}

static CACHE: Lazy<Mutex<Cache>> = Lazy::new(|| {
    Mutex::new(Cache {
        signature: None,
        overrides: HashMap::new(),
        required_by: HashMap::new(),
    })
});

/// Wylde repo root. Mirrors Python's `parents[3]` walk from
/// `model_registry/_service_manifests.py`. Since we can't derive a
/// similar path from a Rust source file at runtime, fall back to
/// `WYLDE_ROOT` env var (set by Lifecycle when it spawns us) or `cwd`.
pub fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Find every service `manifest.json` under the recognised roots. A
/// service manifest sits at `<service>/manifest.json` (top-level of the
/// service folder) or at `<service>/<sub>/manifest.json` for nested
/// services. Tool manifests under `Core/harness/tooling/tools/` are not
/// reached because we don't scan `Core`.
fn candidate_manifests() -> Vec<PathBuf> {
    let root = wylde_root();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for name in SERVICE_ROOTS {
        let service_root = root.join(name);
        if !service_root.is_dir() {
            continue;
        }
        // Direct child manifest first.
        let direct = service_root.join("manifest.json");
        if direct.is_file() && seen.insert(direct.clone()) {
            paths.push(direct);
        }
        // One level deep for `Voice/_wylde_voice`-style services.
        let Ok(it) = std::fs::read_dir(&service_root) else {
            continue;
        };
        for entry in it.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let child_name = entry.file_name();
            let Some(child_str) = child_name.to_str() else {
                continue;
            };
            if child_str.starts_with("__") || child_str.starts_with('.') {
                continue;
            }
            let m = entry.path().join("manifest.json");
            if m.is_file() && seen.insert(m.clone()) {
                paths.push(m);
            }
        }
    }
    paths
}

fn mtime_epoch(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn scan_signature(paths: &[PathBuf]) -> Signature {
    let mut sigs: Signature = Vec::with_capacity(paths.len());
    for p in paths {
        let Ok(meta) = std::fs::metadata(p) else {
            continue;
        };
        sigs.push((p.clone(), mtime_epoch(&meta), meta.len()));
    }
    sigs.sort();
    sigs
}

fn coerce_kind(value: &Value) -> Option<Kind> {
    value.as_str().and_then(Kind::parse)
}

fn read_one(manifest: &Path) -> (String, Vec<(String, Kind)>) {
    let parent_name = manifest
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_owned();
    let Ok(raw) = std::fs::read_to_string(manifest) else {
        return (parent_name, Vec::new());
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return (parent_name, Vec::new());
    };
    let Some(map) = data.as_object() else {
        return (parent_name, Vec::new());
    };
    let service = map
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(parent_name);
    let Some(models_raw) = map.get("models").and_then(Value::as_array) else {
        return (service, Vec::new());
    };
    let mut out: Vec<(String, Kind)> = Vec::new();
    for spec in models_raw {
        let Some(spec_map) = spec.as_object() else {
            continue;
        };
        let model_id = spec_map
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        let Some(kind) = spec_map.get("kind").and_then(coerce_kind) else {
            continue;
        };
        if model_id.is_empty() {
            continue;
        }
        out.push((model_id, kind));
    }
    (service, out)
}

fn build_declarations() -> (HashMap<String, Kind>, HashMap<String, Vec<String>>) {
    let mut overrides: HashMap<String, Kind> = HashMap::new();
    let mut required_by: HashMap<String, Vec<String>> = HashMap::new();
    for manifest in candidate_manifests() {
        let (service, decls) = read_one(&manifest);
        for (model_id, kind) in decls {
            match overrides.get(&model_id) {
                Some(existing) if *existing != kind => {
                    // First service to declare wins, matching Python's
                    // "keeping first" semantics.
                }
                Some(_) | None => {
                    overrides.entry(model_id.clone()).or_insert(kind);
                }
            }
            required_by.entry(model_id).or_default().push(service.clone());
        }
    }
    (overrides, required_by)
}

/// Return cached `(overrides, required_by)` from all service manifests.
/// Cached on the manifests' `(path, mtime, size)` signature, same
/// template as the HF scanner. `force=true` rebuilds.
pub fn load_declarations(
    force: bool,
) -> (HashMap<String, Kind>, HashMap<String, Vec<String>>) {
    let paths = candidate_manifests();
    let sig = scan_signature(&paths);
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if !force {
        if let Some(prev) = &guard.signature {
            if *prev == sig {
                return (guard.overrides.clone(), guard.required_by.clone());
            }
        }
    }
    let (overrides, required_by) = build_declarations();
    guard.overrides = overrides.clone();
    guard.required_by = required_by.clone();
    guard.signature = Some(sig);
    (overrides, required_by)
}

/// Drop the cached scan so the next call rebuilds.
pub fn invalidate_cache() {
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.signature = None;
    guard.overrides.clear();
    guard.required_by.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    struct WyldeRootEnv {
        _guard: MutexGuard<'static, ()>,
        _td: TempDir,
        prior: Option<std::ffi::OsString>,
    }

    impl WyldeRootEnv {
        fn new() -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().expect("tempdir");
            let prior = std::env::var_os("WYLDE_ROOT");
            std::env::set_var("WYLDE_ROOT", td.path());
            invalidate_cache();
            Self {
                _guard: guard,
                _td: td,
                prior,
            }
        }

        fn root(&self) -> &Path {
            self._td.path()
        }
    }

    impl Drop for WyldeRootEnv {
        fn drop(&mut self) {
            invalidate_cache();
            match self.prior.take() {
                Some(v) => std::env::set_var("WYLDE_ROOT", v),
                None => std::env::remove_var("WYLDE_ROOT"),
            }
        }
    }

    fn write_manifest(at: &Path, service: &str, models: &[(&str, &str)]) {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        let arr: Vec<_> = models
            .iter()
            .map(|(id, kind)| {
                serde_json::json!({"id": id, "kind": kind, "required": true})
            })
            .collect();
        let m = serde_json::json!({
            "name": service,
            "models": arr,
        });
        std::fs::write(at, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    }

    #[test]
    fn empty_root_returns_empty_decls() {
        let _env = WyldeRootEnv::new();
        let (o, r) = load_declarations(true);
        assert!(o.is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn manifest_at_top_level_of_service_is_picked_up() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Voice/manifest.json"),
            "voice",
            &[
                ("openai/whisper-small", "stt"),
                ("rhasspy/piper-voices", "tts"),
            ],
        );
        let (overrides, required) = load_declarations(true);
        assert_eq!(overrides.get("openai/whisper-small"), Some(&Kind::Stt));
        assert_eq!(overrides.get("rhasspy/piper-voices"), Some(&Kind::Tts));
        assert_eq!(
            required.get("openai/whisper-small"),
            Some(&vec!["voice".to_owned()])
        );
    }

    #[test]
    fn nested_one_level_manifest_is_picked_up() {
        let env = WyldeRootEnv::new();
        // Voice/_wylde_voice/manifest.json — one level deep.
        write_manifest(
            &env.root().join("Voice/_wylde_voice/manifest.json"),
            "voice-internal",
            &[("openai/whisper-large-v3", "stt")],
        );
        let (overrides, required) = load_declarations(true);
        assert_eq!(overrides.get("openai/whisper-large-v3"), Some(&Kind::Stt));
        assert_eq!(
            required.get("openai/whisper-large-v3"),
            Some(&vec!["voice-internal".to_owned()])
        );
    }

    #[test]
    fn dunder_subdirs_are_skipped() {
        let env = WyldeRootEnv::new();
        // __pycache__/manifest.json — should be ignored.
        write_manifest(
            &env.root().join("Voice/__pycache__/manifest.json"),
            "pycache",
            &[("ignored/model", "llm")],
        );
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn bad_kind_dropped_silently() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Voice/manifest.json"),
            "voice",
            &[("openai/whisper-small", "audio")], // unknown kind
        );
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn first_declaration_wins_on_conflict() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Voice/manifest.json"),
            "voice",
            &[("multi/model", "stt")],
        );
        write_manifest(
            &env.root().join("Gateway/manifest.json"),
            "gateway",
            &[("multi/model", "llm")],
        );
        let (overrides, required) = load_declarations(true);
        // Voice scans first (alphabetical in SERVICE_ROOTS); its kind wins.
        assert_eq!(overrides.get("multi/model"), Some(&Kind::Stt));
        // Both services are still recorded in required_by.
        let req = required.get("multi/model").cloned().unwrap_or_default();
        assert_eq!(req.len(), 2);
        assert!(req.contains(&"voice".to_owned()));
        assert!(req.contains(&"gateway".to_owned()));
    }

    #[test]
    fn manifest_with_no_models_array_is_tolerated() {
        let env = WyldeRootEnv::new();
        let path = env.root().join("Voice/manifest.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"name": "voice"}"#).unwrap();
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn bad_json_is_tolerated() {
        let env = WyldeRootEnv::new();
        let path = env.root().join("Voice/manifest.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn cache_invalidate_forces_rebuild() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Voice/manifest.json"),
            "voice",
            &[("openai/whisper-small", "stt")],
        );
        let _ = load_declarations(true);
        invalidate_cache();
        write_manifest(
            &env.root().join("Voice/manifest.json"),
            "voice",
            &[
                ("openai/whisper-small", "stt"),
                ("rhasspy/piper-voices", "tts"),
            ],
        );
        let (overrides, _) = load_declarations(false);
        // After invalidation + manifest change, both models should appear.
        assert_eq!(overrides.len(), 2);
    }
}
