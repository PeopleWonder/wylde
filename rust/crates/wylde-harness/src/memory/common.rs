//! Internal helpers shared by the harness memory layer. Rust port of
//! `Core/harness/memory/_common.py`.
//!
//! Three small concerns:
//!
//! * Path resolution ([`wylde_root`], [`data_dir`], [`conversations_dir`]).
//! * Memgraph service identity (slice 7.E will use this).
//! * Embedding tunables (slice 7.B/D will use these).
//!
//! Env-var precedence matches Python exactly: `WYLDE_DATA_DIR` →
//! `DATA_DIR` → `<wylde_root>/.wylde/data`.
//!
//! ## Why path lookups aren't cached
//!
//! Python computes these at import time so they're effectively cached
//! per-process. The Rust port resolves on every call so tests can swap
//! `WYLDE_DATA_DIR` per-test without needing a shared `OnceLock` reset.
//! The cost is a couple of env-var reads + a `PathBuf` join per memory
//! op — negligible against the disk IO that follows. In a service
//! process the env vars never change after boot anyway, so this is
//! pure correctness, not regression.

use std::path::{Path, PathBuf};

/// `Wylde/` — repo root. Matches Python's
/// `Path(__file__).resolve().parents[3]`. We can't derive a similar
/// path from a Rust source file location at runtime, so fall back to
/// `WYLDE_ROOT` (set by Lifecycle when it spawns us) or `cwd`.
pub fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// On-disk store root (convention A). Precedence: `WYLDE_DATA_DIR` →
/// `DATA_DIR` → `<wylde_root>/.wylde/data`. Delegates to the ONE canonical
/// resolver (#138) — this used to be a verbatim copy of that body.
pub use wylde_shared::paths::data_dir;

/// One JSON file per conversation lives here. Mirrors Python's
/// `CONVERSATIONS_DIR`.
pub fn conversations_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("CONVERSATIONS_DIR") {
        PathBuf::from(v)
    } else {
        data_dir().join("conversations")
    }
}

/// Create `p` (and parents) if missing. Returns the path for chaining.
/// Mirrors Python's `ensure_dir`.
pub fn ensure_dir(p: &Path) -> std::io::Result<&Path> {
    std::fs::create_dir_all(p)?;
    Ok(p)
}

// ── Memgraph service identity (slice 7.E will consume these) ──────────

/// Memgraph IPC service name. Default `wylde-memgraph`; override via
/// `WYLDE_MEMGRAPH_SERVICE`. Mirrors Python's `MEMGRAPH_SERVICE_NAME`.
pub fn memgraph_service_name() -> String {
    std::env::var("WYLDE_MEMGRAPH_SERVICE").unwrap_or_else(|_| "wylde-memgraph".to_owned())
}

/// Windows named-pipe path for Memgraph (`\\.\pipe\<service-name>`).
/// Mirrors Python's `MEMGRAPH_PIPE_NAME`.
pub fn memgraph_pipe_name() -> String {
    format!(r"\\.\pipe\{}", memgraph_service_name())
}

// ── Embedding tunables (slice 7.B/D will consume) ─────────────────────

/// The effective embedder — ONE definition, unified with the reasoning
/// slot store in S2: `WYLDE_EMBED_MODEL` (the env override) →
/// `ReasoningConfig.slots.embedder` (the settings-backed source) →
/// the built-in default. The slot's default is the same tag the old
/// env-only fallback used, so a fresh install resolves identically.
pub fn embed_model() -> String {
    if let Ok(v) = std::env::var("WYLDE_EMBED_MODEL") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    let slot = crate::turn::reasoning::ReasoningConfig::current()
        .slots
        .embedder;
    if slot.trim().is_empty() {
        crate::turn::reasoning::config::DEFAULT_EMBED_MODEL.to_owned()
    } else {
        slot
    }
}

pub fn embed_native_dim() -> usize {
    std::env::var("WYLDE_EMBED_NATIVE_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768)
}

pub fn embed_dim() -> usize {
    std::env::var("WYLDE_EMBED_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(768)
}

/// Process-wide mutex guarding `WYLDE_DATA_DIR` mutation in tests.
///
/// Several sub-modules (`workspaces::test_support`, `long_term::test_support`,
/// and any future tier) all rebind `WYLDE_DATA_DIR` to a per-test
/// tempdir. The env var is process-wide, so they MUST serialize against
/// the same lock. We expose this single static so each `TestEnv` helper
/// can lock it without each defining its own (which would race).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// #138 — a REAL fallback-shape gate. The harness `data_dir` is now the ONE
    /// canonical resolver (`wylde_shared::paths`); its env-free fallback is
    /// `<root>/.wylde/data`. The old body here asserted only `!p.is_empty()` —
    /// green under any convention, including a regression to `.` — and its
    /// docstring's "cached at first access" claim was false (it resolves per
    /// call). This pins the actual `.wylde/data` shape via the pure helper, so a
    /// drift to a different root or layout turns it red.
    #[test]
    fn data_dir_falls_back_to_dot_wylde_under_root() {
        // Env-free: pin the canonical fallback shape via the pure helper the
        // harness `data_dir` delegates to. (We avoid mutating `WYLDE_ROOT` here
        // because harness memory tests share the process env without a lock.)
        let root = Path::new("C:/estate-root");
        assert_eq!(
            wylde_shared::paths::data_dir_under(root),
            root.join(".wylde").join("data"),
        );
    }

    #[test]
    fn ensure_dir_creates_nested_parents() {
        let td = tempdir().unwrap();
        let nested = td.path().join("a/b/c");
        let returned = ensure_dir(&nested).unwrap();
        assert_eq!(returned, nested);
        assert!(nested.is_dir());
    }

    #[test]
    fn memgraph_pipe_name_uses_service_name() {
        // Snapshot + restore so the OnceLock-style env reads don't
        // leak between tests.
        let prev = std::env::var("WYLDE_MEMGRAPH_SERVICE").ok(); // wylde-check: discard-result-ok
        std::env::set_var("WYLDE_MEMGRAPH_SERVICE", "test-memgraph");
        let name = memgraph_pipe_name();
        assert_eq!(name, r"\\.\pipe\test-memgraph");
        match prev {
            Some(v) => std::env::set_var("WYLDE_MEMGRAPH_SERVICE", v),
            None => std::env::remove_var("WYLDE_MEMGRAPH_SERVICE"),
        }
    }

    #[test]
    fn embed_dim_falls_back_to_768_when_unset() {
        let prev = std::env::var("WYLDE_EMBED_DIM").ok(); // wylde-check: discard-result-ok
        std::env::remove_var("WYLDE_EMBED_DIM");
        assert_eq!(embed_dim(), 768);
        if let Some(v) = prev {
            std::env::set_var("WYLDE_EMBED_DIM", v);
        }
    }
}
