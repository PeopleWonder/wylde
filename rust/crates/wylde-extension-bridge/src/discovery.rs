//! Filesystem walk for extension discovery.
//!
//! Scans `<extensions_dir>` for direct subdirectories containing an
//! `mcp-server.json`. Reserved folder names match the Python
//! Extensions convention: `extension_bridge`, `_shim`, anything starting
//! with `_` or `.`.
//!
//! Each scan is cached by an `(path, mtime, size)` signature across all
//! discovered manifests. Calling [`discover`] when nothing has changed
//! returns the cached list with no re-parse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::manifest::{load_extension, ExtensionRecord, ManifestError};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Signature {
    entries: Vec<(PathBuf, u128, u64)>, // (path, mtime_unix_nanos, size)
}

#[derive(Default)]
struct Cache {
    signature: Option<Signature>,
    extensions: BTreeMap<String, ExtensionRecord>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Load all extensions under `extensions_dir`, returning them keyed by
/// extension name (the `name` field from `mcp-server.json`, NOT the
/// folder name). Parse errors for individual extensions are logged and
/// elided from the result so one broken manifest doesn't take the host
/// offline.
pub fn discover(extensions_dir: &Path) -> BTreeMap<String, ExtensionRecord> {
    let sig = scan_signature(extensions_dir);
    let mut guard = CACHE.lock().expect("discovery cache poisoned");
    if let Some(cache) = guard.as_ref() {
        if cache.signature.as_ref() == Some(&sig) {
            return cache.extensions.clone();
        }
    }
    let mut out: BTreeMap<String, ExtensionRecord> = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(extensions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_reserved(name) {
                continue;
            }
            match load_extension(&path) {
                Ok(rec) => {
                    out.insert(rec.manifest.name.clone(), rec);
                }
                Err(ManifestError::NotFound(_)) => {
                    // Folder has no mcp-server.json — silently skip; it
                    // may be a pure-Python legacy extension that hasn't
                    // been migrated yet. Those are visible to the
                    // legacy Python bridge during the strangler window.
                }
                Err(e) => {
                    tracing::warn!("discovery: failed to load {:?}: {}", path, e);
                }
            }
        }
    }
    *guard = Some(Cache {
        signature: Some(sig),
        extensions: out.clone(),
    });
    out
}

/// Drop the cache so the next [`discover`] call re-walks the filesystem.
/// Call after [`crate::manifest::write_enabled`] mutates a manifest.
pub fn invalidate_cache() {
    if let Ok(mut guard) = CACHE.lock() {
        *guard = None;
    }
}

fn is_reserved(name: &str) -> bool {
    matches!(name, "extension_bridge" | "_shim") || name.starts_with('_') || name.starts_with('.')
}

fn scan_signature(extensions_dir: &Path) -> Signature {
    let mut entries: Vec<(PathBuf, u128, u64)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(extensions_dir) {
        for ent in dir.flatten() {
            let path = ent.path();
            let mcp = path.join("mcp-server.json");
            if !mcp.exists() {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&mcp) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                entries.push((mcp, mtime, meta.len()));
            }
        }
    }
    entries.sort();
    Signature { entries }
}

/// Test-only: clear the cache (use to make tests deterministic).
#[doc(hidden)]
pub fn reset_for_tests() {
    invalidate_cache();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_ext(dir: &Path, name: &str, payload: &str) {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("mcp-server.json"), payload).unwrap();
    }

    #[test]
    fn discovers_extensions_skipping_reserved() {
        reset_for_tests();
        let td = TempDir::new().unwrap();
        write_ext(
            td.path(),
            "good",
            r#"{"name":"good","transport":"stdio","command":["x"]}"#,
        );
        write_ext(
            td.path(),
            "extension_bridge",
            r#"{"name":"bridge","transport":"stdio","command":["x"]}"#,
        );
        write_ext(
            td.path(),
            "_shim",
            r#"{"name":"shim","transport":"stdio","command":["x"]}"#,
        );
        write_ext(
            td.path(),
            ".hidden",
            r#"{"name":"hidden","transport":"stdio","command":["x"]}"#,
        );
        let m = discover(td.path());
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("good"));
    }

    #[test]
    fn cache_hits_when_signature_unchanged() {
        reset_for_tests();
        let td = TempDir::new().unwrap();
        write_ext(
            td.path(),
            "a",
            r#"{"name":"a","transport":"stdio","command":["x"]}"#,
        );
        let first = discover(td.path());
        let second = discover(td.path());
        assert_eq!(first.len(), second.len());
    }
}
