//! Walk the HuggingFace cache, enumerate models, cache by signature.
//! Rust port of `Core/harness/model_registry/_hf_scanner.py`.
//!
//! The HF Hub library lays models out as:
//!
//! ```text
//! ~/.cache/huggingface/hub/
//!     models--microsoft--Florence-2-large/
//!         blobs/...
//!         refs/main
//!         snapshots/<sha>/...
//! ```
//!
//! This module walks `models--*/`, parses the repo name back out of the
//! dasherised folder name, sums file sizes, and emits `ModelEntry`
//! records. Cache invalidation follows the same template as the Python
//! version: snapshot `(path, mtime, size)` per directory and rebuild
//! only on mismatch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use once_cell::sync::Lazy;

use crate::model_registry::heuristics::infer_kind;
use crate::model_registry::types::{default_chat_visible, Kind, ModelEntry};

const MODELS_PREFIX: &str = "models--";

type Signature = Vec<(PathBuf, u64, u64)>;

struct Cache {
    signature: Option<Signature>,
    entries: Vec<ModelEntry>,
}

static CACHE: Lazy<Mutex<Cache>> = Lazy::new(|| {
    Mutex::new(Cache {
        signature: None,
        entries: Vec::new(),
    })
});

/// Resolve the HF Hub cache root in the order huggingface_hub itself
/// uses. Order: `HF_HUB_CACHE` → `HUGGINGFACE_HUB_CACHE` → `HF_HOME`/hub
/// → `~/.cache/huggingface/hub`. We don't link huggingface_hub here so
/// the registry stays usable on systems that haven't installed it yet.
pub fn hub_root() -> PathBuf {
    for env in ["HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE"] {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return expand_user(&v);
            }
        }
    }
    if let Ok(v) = std::env::var("HF_HOME") {
        if !v.is_empty() {
            return expand_user(&v).join("hub");
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("huggingface")
        .join("hub")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn expand_user(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~") {
        if let Some(home) = home_dir() {
            // ``~`` → home; ``~/foo`` → home/foo.
            let trimmed = rest.trim_start_matches(['/', '\\']);
            if trimmed.is_empty() {
                return home;
            }
            return home.join(trimmed);
        }
    }
    PathBuf::from(s)
}

/// `models--microsoft--Florence-2-large` → `microsoft/Florence-2-large`.
///
/// huggingface_hub replaces every `/` in the repo name with `--` —
/// converting all `--` separators back gives the canonical id even for
/// nested repos like `rhasspy/piper-voices`.
fn parse_repo_name(folder_name: &str) -> Option<String> {
    let body = folder_name.strip_prefix(MODELS_PREFIX)?;
    if body.is_empty() {
        return None;
    }
    Some(body.replace("--", "/"))
}

fn mtime_epoch(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn atime_epoch_f64(meta: &std::fs::Metadata) -> Option<f64> {
    meta.accessed()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

/// Sum file sizes and capture the most-recent atime under `path`.
fn dir_size_and_atime(path: &Path) -> (u64, Option<f64>) {
    let mut total: u64 = 0;
    let mut latest: Option<f64> = None;
    let blobs = path.join("blobs");
    if blobs.is_dir() {
        if let Ok(it) = std::fs::read_dir(&blobs) {
            for entry in it.flatten() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                total += meta.len();
                if let Some(t) = atime_epoch_f64(&meta) {
                    latest = Some(match latest {
                        Some(prev) if prev > t => prev,
                        _ => t,
                    });
                }
            }
        }
        return (total, latest);
    }
    // Older cache layout: no blobs/ tree. Walk recursively, skip dir
    // symlinks to avoid double-counting.
    walk_size(path, &mut total, &mut latest);
    (total, latest)
}

fn walk_size(path: &Path, total: &mut u64, latest: &mut Option<f64>) {
    let Ok(it) = std::fs::read_dir(path) else {
        return;
    };
    for entry in it.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let p = entry.path();
        if file_type.is_dir() {
            walk_size(&p, total, latest);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        *total += meta.len();
        if let Some(t) = atime_epoch_f64(&meta) {
            *latest = Some(match *latest {
                Some(prev) if prev > t => prev,
                _ => t,
            });
        }
    }
}

/// (path, mtime, size) per `models--*` dir, sorted for stability.
/// Missing hub_dir → empty list, treated as a valid cacheable state.
fn scan_signature(hub_dir: &Path) -> Signature {
    if !hub_dir.is_dir() {
        return Vec::new();
    }
    let Ok(it) = std::fs::read_dir(hub_dir) else {
        return Vec::new();
    };
    let mut sigs: Signature = Vec::new();
    for entry in it.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with(MODELS_PREFIX) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        sigs.push((entry.path(), mtime_epoch(&meta), meta.len()));
    }
    sigs.sort();
    sigs
}

fn build_entries(
    hub_dir: &Path,
    overrides: &HashMap<String, Kind>,
    required_by: &HashMap<String, Vec<String>>,
) -> Vec<ModelEntry> {
    let mut entries: Vec<ModelEntry> = Vec::new();
    if !hub_dir.is_dir() {
        return entries;
    }
    let Ok(it) = std::fs::read_dir(hub_dir) else {
        return entries;
    };
    for entry in it.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with(MODELS_PREFIX) {
            continue;
        }
        let Some(repo) = parse_repo_name(name_str) else {
            continue;
        };
        let path = entry.path();
        let (size, atime) = dir_size_and_atime(&path);
        let kind = overrides
            .get(&repo)
            .copied()
            .unwrap_or_else(|| infer_kind(&repo));
        entries.push(ModelEntry {
            id: repo.clone(),
            kind,
            path: Some(path.to_string_lossy().into_owned()),
            size_bytes: size,
            loaded: false,
            provider: "huggingface".to_owned(),
            required_by: required_by.get(&repo).cloned().unwrap_or_default(),
            profile: None,
            last_accessed: atime,
            chat_visible: default_chat_visible(kind),
        });
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Return `ModelEntry`s for every model in the HF cache. Cached on the
/// `(path, mtime, size)` signature of the hub root's `models--*`
/// children. `force=true` skips the cache (used by `refresh_cache`).
pub fn scan_hf_cache(
    overrides: &HashMap<String, Kind>,
    required_by: &HashMap<String, Vec<String>>,
    force: bool,
) -> Vec<ModelEntry> {
    let hub_dir = hub_root();
    let sig = scan_signature(&hub_dir);
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if !force {
        if let Some(prev) = &guard.signature {
            if *prev == sig {
                return guard.entries.clone();
            }
        }
    }
    let entries = build_entries(&hub_dir, overrides, required_by);
    guard.entries = entries.clone();
    guard.signature = Some(sig);
    entries
}

/// Drop the cached scan so the next call rebuilds.
pub fn invalidate_cache() {
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.signature = None;
    guard.entries.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// HF env vars + lock — keeps `hub_root()` deterministic across the
    /// scanner tests. Holds `TEST_ENV_LOCK` because every other env-var
    /// test in this crate uses the same lock.
    struct HfEnv {
        _guard: MutexGuard<'static, ()>,
        _td: TempDir,
        prior_hub: Option<std::ffi::OsString>,
        prior_huggingface_hub: Option<std::ffi::OsString>,
        prior_hf_home: Option<std::ffi::OsString>,
    }

    impl HfEnv {
        fn new() -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().expect("tempdir");
            let prior_hub = std::env::var_os("HF_HUB_CACHE");
            let prior_huggingface_hub = std::env::var_os("HUGGINGFACE_HUB_CACHE");
            let prior_hf_home = std::env::var_os("HF_HOME");
            std::env::set_var("HF_HUB_CACHE", td.path());
            std::env::remove_var("HUGGINGFACE_HUB_CACHE");
            std::env::remove_var("HF_HOME");
            invalidate_cache();
            Self {
                _guard: guard,
                _td: td,
                prior_hub,
                prior_huggingface_hub,
                prior_hf_home,
            }
        }

        fn hub(&self) -> &Path {
            self._td.path()
        }
    }

    impl Drop for HfEnv {
        fn drop(&mut self) {
            invalidate_cache();
            match self.prior_hub.take() {
                Some(v) => std::env::set_var("HF_HUB_CACHE", v),
                None => std::env::remove_var("HF_HUB_CACHE"),
            }
            if let Some(v) = self.prior_huggingface_hub.take() {
                std::env::set_var("HUGGINGFACE_HUB_CACHE", v);
            }
            if let Some(v) = self.prior_hf_home.take() {
                std::env::set_var("HF_HOME", v);
            }
        }
    }

    fn make_model(hub: &Path, folder: &str, blob_bytes: &[u8]) {
        let dir = hub.join(folder);
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("data.bin"), blob_bytes).unwrap();
    }

    #[test]
    fn parse_repo_name_handles_org_slash_repo() {
        assert_eq!(
            parse_repo_name("models--microsoft--Florence-2-large"),
            Some("microsoft/Florence-2-large".to_owned())
        );
    }

    #[test]
    fn parse_repo_name_handles_nested_dashes() {
        // rhasspy/piper-voices — single dash inside the repo name. The
        // ``--`` separator marks the org boundary; the surviving ``-``
        // should NOT be turned into a ``/``.
        assert_eq!(
            parse_repo_name("models--rhasspy--piper-voices"),
            Some("rhasspy/piper-voices".to_owned())
        );
    }

    #[test]
    fn parse_repo_name_rejects_other_prefixes() {
        assert_eq!(parse_repo_name("datasets--squad"), None);
        assert_eq!(parse_repo_name("models--"), None);
    }

    #[test]
    fn scan_empty_hub_returns_empty_list() {
        let env = HfEnv::new();
        let overrides = HashMap::new();
        let required = HashMap::new();
        let out = scan_hf_cache(&overrides, &required, true);
        assert!(out.is_empty());
        assert_eq!(hub_root(), env.hub());
    }

    #[test]
    fn scan_picks_up_models_dirs_only() {
        let env = HfEnv::new();
        make_model(env.hub(), "models--microsoft--Florence-2-large", b"abcdef");
        // Non-models directory should be ignored.
        std::fs::create_dir_all(env.hub().join("datasets--squad")).unwrap();
        let overrides = HashMap::new();
        let required = HashMap::new();
        let out = scan_hf_cache(&overrides, &required, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "microsoft/Florence-2-large");
        assert_eq!(out[0].kind, Kind::Vision);
        assert_eq!(out[0].size_bytes, 6);
        assert_eq!(out[0].provider, "huggingface");
        assert!(out[0].path.is_some());
    }

    #[test]
    fn manifest_overrides_beat_heuristic() {
        let env = HfEnv::new();
        make_model(env.hub(), "models--vendor--mystery-model", b"x");
        let mut overrides = HashMap::new();
        overrides.insert("vendor/mystery-model".to_owned(), Kind::Embed);
        let mut required = HashMap::new();
        required.insert(
            "vendor/mystery-model".to_owned(),
            vec!["voice".to_owned()],
        );
        let out = scan_hf_cache(&overrides, &required, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, Kind::Embed);
        assert_eq!(out[0].required_by, vec!["voice".to_owned()]);
    }

    #[test]
    fn cache_signature_short_circuits_when_unchanged() {
        let env = HfEnv::new();
        make_model(env.hub(), "models--openai--whisper-small", b"abcd");
        let overrides = HashMap::new();
        let required = HashMap::new();
        let first = scan_hf_cache(&overrides, &required, true);
        assert_eq!(first.len(), 1);

        // Modify the entry under-the-hood but DON'T touch the mtime of
        // the top folder (test exercises the cache hit). Then ask
        // again with force=false — should return the cached entry.
        let second = scan_hf_cache(&overrides, &required, false);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, first[0].id);
    }

    #[test]
    fn invalidate_clears_cached_signature() {
        let env = HfEnv::new();
        make_model(env.hub(), "models--openai--whisper-small", b"x");
        let overrides = HashMap::new();
        let required = HashMap::new();
        let _ = scan_hf_cache(&overrides, &required, false);
        invalidate_cache();
        // Mutate the cache root and re-scan; should pick up the change.
        make_model(env.hub(), "models--rhasspy--piper-voices", b"y");
        let out = scan_hf_cache(&overrides, &required, false);
        // Two models now.
        assert_eq!(out.len(), 2);
    }
}
