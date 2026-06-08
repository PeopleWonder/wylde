//! Internal filesystem + embedding helpers for `wylde-workspaces`.
//!
//! Relocated from the harness `crate::memory::common` (Slice 0b) so the
//! workspace stores keep **byte-identical on-disk locations** after the
//! service extraction — the registry/persona/rag code that moved here
//! resolves its data dir through [`data_dir`], and that must point at the
//! same place the harness used (`<data_dir>/workspaces/`) so existing data
//! is found and the harness's in-process compat-shim fallback and the new
//! service agree on the path. Env-var precedence is therefore copied
//! verbatim: `WYLDE_DATA_DIR` → `DATA_DIR` → `<wylde_root>/.wylde/data`.
//!
//! Resolution is per-call (not cached) so tests can rebind `WYLDE_DATA_DIR`
//! to a tempdir; in a service process the env never changes after boot.

use std::path::{Path, PathBuf};

/// Install / repo root. `WYLDE_ROOT` (set by Lifecycle when it spawns us)
/// or the cwd fallback. Mirrors the harness `memory::common::wylde_root`.
pub fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// On-disk store root. Override precedence: `WYLDE_DATA_DIR` → `DATA_DIR`
/// → `<wylde_root>/.wylde/data`. **Identical** to the harness resolver so
/// the relocated stores keep their existing files.
pub fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        PathBuf::from(v)
    } else if let Some(v) = std::env::var_os("DATA_DIR") {
        PathBuf::from(v)
    } else {
        wylde_root().join(".wylde").join("data")
    }
}

/// Create `p` (and parents) if missing. Returns the path for chaining.
pub fn ensure_dir(p: &Path) -> std::io::Result<&Path> {
    std::fs::create_dir_all(p)?;
    Ok(p)
}

// ── Embedding tunables (consumed by `crate::embeddings`) ──────────────────

/// Embedding model name. Default `nomic-embed-text`; override via
/// `WYLDE_EMBED_MODEL`.
pub fn embed_model() -> String {
    std::env::var("WYLDE_EMBED_MODEL").unwrap_or_else(|_| "nomic-embed-text".to_owned())
}

/// Native output dimension of the embedding model (pre-Matryoshka).
/// Default 768; override via `WYLDE_EMBED_NATIVE_DIM`.
pub fn embed_native_dim() -> usize {
    std::env::var("WYLDE_EMBED_NATIVE_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768)
}

/// Target dimension after Matryoshka truncation. Default 768 (= native, so
/// no truncation); override via `WYLDE_EMBED_DIM`.
pub fn embed_dim() -> usize {
    std::env::var("WYLDE_EMBED_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768)
}

/// Process-wide mutex guarding `WYLDE_DATA_DIR` (and the `WYLDE_EMBED_*`
/// dims) mutation in tests. Every `TestEnv` / dim-pinning helper locks this
/// so concurrent tests in the same binary don't observe each other's env.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn data_dir_resolves_to_a_nonempty_path() {
        assert!(!data_dir().as_os_str().is_empty());
    }

    #[test]
    fn ensure_dir_creates_nested_parents() {
        let td = tempdir().unwrap();
        let nested = td.path().join("a/b/c");
        assert_eq!(ensure_dir(&nested).unwrap(), nested);
        assert!(nested.is_dir());
    }

    #[test]
    fn embed_dims_fall_back_to_768() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_n = std::env::var("WYLDE_EMBED_NATIVE_DIM").ok();
        let prev_d = std::env::var("WYLDE_EMBED_DIM").ok();
        std::env::remove_var("WYLDE_EMBED_NATIVE_DIM");
        std::env::remove_var("WYLDE_EMBED_DIM");
        assert_eq!(embed_native_dim(), 768);
        assert_eq!(embed_dim(), 768);
        if let Some(v) = prev_n {
            std::env::set_var("WYLDE_EMBED_NATIVE_DIM", v);
        }
        if let Some(v) = prev_d {
            std::env::set_var("WYLDE_EMBED_DIM", v);
        }
    }
}
