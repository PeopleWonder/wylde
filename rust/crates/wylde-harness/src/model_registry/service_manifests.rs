//! Read `models` declarations from service manifests. Rust port of
//! `Core/harness/model_registry/_service_manifests.py`.
//!
//! Each service in the out-of-tree `Services/` bucket may ship a
//! `manifest.json` with a `models` array. This module collects every
//! such declaration and returns:
//!
//! * an `overrides` map (model id → kind) — wins over the heuristic.
//! * a `required_by` map (model id → list of service names) — surfaced
//!   on the entry for the GUI.
//!
//! Manifests without a `models` key are silently skipped.
//!
//! ## Scan roots are *discovered*, not listed (#125)
//!
//! The scan used to walk a hand-kept list of pre-cutover top-level folder
//! names (`Voice`, `Gateway`, `N8N`, …), most of which no longer exist,
//! and it did **not** include `Services/` — so a `Services/<svc>/manifest.json`
//! that declared a model was invisible to the model registry, silently, by
//! construction (a manifest with no `models` key is legitimately skipped, so an
//! absent root looked identical to a no-op). The roots now come from
//! [`wylde_stack::roster::discovered_folders`] — the *same* `Services/` walk
//! the updater, launcher, and lifecycle daemon already follow — so a service
//! dropped into the bucket is covered with no edit here, and honours the same
//! `WYLDE_SERVICES` override.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::model_registry::types::Kind;

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

/// Find every service `manifest.json` in the `Services/` bucket.
///
/// The roots are *discovered*, not listed:
/// [`wylde_stack::roster::discovered_folders`] returns each
/// immediate `Services/<svc>/` folder that carries a readable `manifest.json`
/// (dot/underscore-prefixed folders excluded), the same walk the updater,
/// launcher, and lifecycle registry use. Each such folder's `manifest.json` is
/// a candidate; whether it declares any `models` is decided later in
/// [`read_one`].
fn candidate_manifests() -> Vec<PathBuf> {
    let root = wylde_root();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for folder in wylde_stack::roster::discovered_folders(&root) {
        let manifest = folder.join("manifest.json");
        // `discovered_folders` already guarantees the manifest exists, but the
        // re-check keeps this robust to a folder that vanishes between the walk
        // and here (and is cheap).
        if manifest.is_file() && seen.insert(manifest.clone()) {
            paths.push(manifest);
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
            required_by
                .entry(model_id)
                .or_default()
                .push(service.clone());
        }
    }
    (overrides, required_by)
}

/// Return cached `(overrides, required_by)` from all service manifests.
/// Cached on the manifests' `(path, mtime, size)` signature, same
/// template as the HF scanner. `force=true` rebuilds.
pub fn load_declarations(force: bool) -> (HashMap<String, Kind>, HashMap<String, Vec<String>>) {
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
        prior_root: Option<std::ffi::OsString>,
        prior_services: Option<std::ffi::OsString>,
    }

    impl WyldeRootEnv {
        fn new() -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().expect("tempdir");
            let prior_root = std::env::var_os("WYLDE_ROOT");
            // The `Services/` bucket walk honours a `WYLDE_SERVICES` override
            // (`wylde_stack::roster::resolve_bucket_dir`), which a real dev
            // machine sets to its live install. Clear it so the scan resolves
            // under the tempdir; without this the discovery would read the
            // developer's actual `Services/` and the test wouldn't be hermetic.
            let prior_services = std::env::var_os("WYLDE_SERVICES");
            std::env::set_var("WYLDE_ROOT", td.path());
            std::env::remove_var("WYLDE_SERVICES");
            invalidate_cache();
            Self {
                _guard: guard,
                _td: td,
                prior_root,
                prior_services,
            }
        }

        fn root(&self) -> &Path {
            self._td.path()
        }
    }

    impl Drop for WyldeRootEnv {
        fn drop(&mut self) {
            invalidate_cache();
            match self.prior_root.take() {
                Some(v) => std::env::set_var("WYLDE_ROOT", v),
                None => std::env::remove_var("WYLDE_ROOT"),
            }
            match self.prior_services.take() {
                Some(v) => std::env::set_var("WYLDE_SERVICES", v),
                None => std::env::remove_var("WYLDE_SERVICES"),
            }
        }
    }

    fn write_manifest(at: &Path, service: &str, models: &[(&str, &str)]) {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        let arr: Vec<_> = models
            .iter()
            .map(|(id, kind)| serde_json::json!({"id": id, "kind": kind, "required": true}))
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
    fn services_bucket_manifest_is_seen_by_the_model_registry() {
        // #125B falsification: a service dropped into the `Services/` bucket
        // that declares a model must be visible to the model registry — with no
        // code edit. Before deriving the roots from `discovered_folders` the
        // scan walked a hand-kept list of pre-cutover names that did NOT include
        // `Services/`, so this manifest was silently invisible; reverting to
        // that literal turns this red. Mirrors the discovery seam
        // `wylde_updater/tests/whole_stack_coverage.rs` drives.
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Services/wylde-synthetic/manifest.json"),
            "wylde-synthetic",
            &[("acme/embed-v1", "embed")],
        );
        let (overrides, required) = load_declarations(true);
        assert_eq!(
            overrides.get("acme/embed-v1"),
            Some(&Kind::Embed),
            "a Services/ manifest's model must reach the registry",
        );
        assert_eq!(
            required.get("acme/embed-v1"),
            Some(&vec!["wylde-synthetic".to_owned()])
        );
    }

    #[test]
    fn manifest_at_top_level_of_service_is_picked_up() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Services/Voice/manifest.json"),
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
    fn underscore_prefixed_bucket_children_are_skipped() {
        let env = WyldeRootEnv::new();
        // `discovered_folders` excludes `_`/`.`-prefixed immediate children of
        // the bucket (private/dotfiles) — so a `Services/_staging/` never
        // reaches the model registry, mirroring the daemon's own discovery.
        write_manifest(
            &env.root().join("Services/_staging/manifest.json"),
            "staging",
            &[("ignored/model", "llm")],
        );
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn bad_kind_dropped_silently() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Services/Voice/manifest.json"),
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
            &env.root().join("Services/svc-a/manifest.json"),
            "svc-a",
            &[("multi/model", "stt")],
        );
        write_manifest(
            &env.root().join("Services/svc-b/manifest.json"),
            "svc-b",
            &[("multi/model", "llm")],
        );
        let (overrides, required) = load_declarations(true);
        // `discovered_folders` returns the bucket's children sorted by path, so
        // `svc-a` scans before `svc-b`; the first declaration's kind wins.
        assert_eq!(overrides.get("multi/model"), Some(&Kind::Stt));
        // Both services are still recorded in required_by.
        let req = required.get("multi/model").cloned().unwrap_or_default();
        assert_eq!(req.len(), 2);
        assert!(req.contains(&"svc-a".to_owned()));
        assert!(req.contains(&"svc-b".to_owned()));
    }

    #[test]
    fn manifest_with_no_models_array_is_tolerated() {
        let env = WyldeRootEnv::new();
        let path = env.root().join("Services/Voice/manifest.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"name": "voice"}"#).unwrap();
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn bad_json_is_tolerated() {
        let env = WyldeRootEnv::new();
        let path = env.root().join("Services/Voice/manifest.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let (overrides, _) = load_declarations(true);
        assert!(overrides.is_empty());
    }

    #[test]
    fn cache_invalidate_forces_rebuild() {
        let env = WyldeRootEnv::new();
        write_manifest(
            &env.root().join("Services/Voice/manifest.json"),
            "voice",
            &[("openai/whisper-small", "stt")],
        );
        let _ = load_declarations(true);
        invalidate_cache();
        write_manifest(
            &env.root().join("Services/Voice/manifest.json"),
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
