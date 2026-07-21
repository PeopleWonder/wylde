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

use std::path::Path;

/// Install / repo root and on-disk store root (convention A) — both delegate to
/// the ONE canonical resolver (#138). These used to be verbatim copies of the
/// `wylde_shared::paths` bodies; keeping the re-export means the relocated
/// workspace stores still resolve `<data_dir>/workspaces/` at the byte-identical
/// location the harness used, now from a single source that can't drift.
pub use wylde_shared::paths::{data_dir, wylde_root};

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

    /// #138 — a REAL fallback-shape gate. `data_dir` is now the ONE canonical
    /// resolver; convention A resolves `<root>/.wylde/data`. The old body
    /// asserted only that the path was non-empty, which held under any
    /// convention (including a regression to `.`). Here we drive the ACTUAL
    /// re-exported `data_dir()` under a controlled env (holding the shared lock
    /// so a concurrent `TestEnv` can't perturb `WYLDE_DATA_DIR`) and pin the
    /// exact `<root>/.wylde/data` shape — red on any drift or a stub to `.`.
    #[test]
    fn data_dir_resolves_to_the_dot_wylde_data_shape() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_dd = std::env::var_os("WYLDE_DATA_DIR");
        let prev_d = std::env::var_os("DATA_DIR");
        let prev_root = std::env::var_os("WYLDE_ROOT");
        std::env::remove_var("WYLDE_DATA_DIR");
        std::env::remove_var("DATA_DIR");
        std::env::set_var("WYLDE_ROOT", "C:/estate-root");

        assert_eq!(
            data_dir(),
            Path::new("C:/estate-root").join(".wylde").join("data"),
        );

        match prev_dd {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
        match prev_d {
            Some(v) => std::env::set_var("DATA_DIR", v),
            None => std::env::remove_var("DATA_DIR"),
        }
        match prev_root {
            Some(v) => std::env::set_var("WYLDE_ROOT", v),
            None => std::env::remove_var("WYLDE_ROOT"),
        }
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
