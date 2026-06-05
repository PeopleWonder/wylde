//! `memory/` — long-term, workspace, and short-term memory + RAG.
//! Phase 7 of the Wylde Rust migration. Rust port of
//! `Core/harness/memory/`.
//!
//! ## Scope by slice
//!
//! Phase 7 is large (~9K LOC across the Python module) and lands as a
//! sequence of slices that each ship a coherent subsurface:
//!
//! * **7.A** (SHIPPED) — [`workspaces`] *registry-only* half:
//!   `Workspace` struct + JSON registry + MRU cap + persona +
//!   delete-from-registry + index-folder rmtree. EXCLUDES `activate`
//!   (would need LanceDB + embedder for the index pass).
//! * **7.B** — workspace indexing: `activate` / `refresh` / `reindex` /
//!   `status` / `search_files`. Requires either a LanceDB Rust client
//!   or a temporary IPC bridge back to Python.
//! * **7.C** — long-term memory (`long_term.py` + `scoring.py` +
//!   `reflection.py`). Importance + recency scoring, supersession
//!   chains, reflection cycles, JSON + LanceDB persistence.
//! * **7.D** — RAG: chunking, embedding (via wylde-ollama),
//!   retrieval, miss log, multihop, decompose, entities, feedback,
//!   gate, cache. Wires the `rag.*` deferred tools as Active.
//! * **7.E** — Memgraph: graph_retrieval + memgraph clients. Wires
//!   `meta.graph_query`.
//! * **7.F** — `scheduler.py` (was flagged as orphan in the language
//!   partition plan; rolled into Phase 7).
//!
//! Each slice keeps the Python implementation alive; the
//! strangler-fig env var [`impl_for`] selects which side serves the
//! action handlers. The default flipped to `rust` on 2026-05-26 with
//! the memgraph cutover (only branch that currently reads this gate);
//! sibling submodules wire their Rust handlers in directly, so the
//! default change does not retroactively cut them over.
//!
//! ## Submodule layout
//!
//! Mirrors `Core/harness/memory/`:
//!
//! * [`common`] — `DATA_DIR`, `ensure_dir`, embed-dim constants,
//!   Memgraph service identity. Equivalent of `_common.py`.
//! * [`workspaces`] — folder model. 7.A.
//! * `long_term/` — 7.C (not yet present).
//! * `rag/` — 7.D (not yet present).
//! * `memgraph/` — 7.E (not yet present).
//!
//! ## Strangler-fig env var
//!
//! [`impl_for`] reads `WYLDE_HARNESS_MEMORY_IMPL`. Default `rust`
//! since the 2026-05-26 memgraph cutover (Bolt is the canonical wire
//! shape; the three Python pipe verbs that diverge in the parity test
//! — `relate` / `unrelate` / `upsert_edge` — were always-broken
//! Python paths, not Rust regressions). Anything other than
//! `python` / `rust` is clamped to `rust` so a typo lands on the
//! canonical path. `python` stays available as the rollback escape
//! hatch during the strangler-fig soak window (2–4 weeks per
//! Wylde convention); the Python `Core/Memgraph/` service is not
//! deleted until that window closes. Mirrors the
//! `Core/harness/pipe/_chat.py::_harness_turn_impl` semantics from
//! Phase 5.A — the Wylde user's standing instruction is one rename window per
//! strangler, fail-safe defaults, and silent fallback at the call
//! site.

pub mod common;
pub mod conversations;
pub mod embeddings;
pub mod long_term;
pub mod memgraph;
pub mod rag;
pub mod short_term;
pub mod vector;
pub mod workspaces;

/// Read `WYLDE_HARNESS_MEMORY_IMPL` once per call. Default `rust`
/// (post-2026-05-26 cutover); unknown values clamp to `rust`. Setting
/// `WYLDE_HARNESS_MEMORY_IMPL=python` is the rollback escape hatch.
///
/// Slice 7.A registers `memory.workspaces.*` actions on the Rust pipe
/// unconditionally — they're new wire surface, not a re-bind of any
/// existing Python action. The Python `_rag_workspaces.py` handlers
/// stay canonical; a future slice will add a forward layer there that
/// reads this env var and routes the registry-side actions to Rust
/// once parity tests cover the JSON shape.
pub fn impl_for() -> &'static str {
    let raw = std::env::var("WYLDE_HARNESS_MEMORY_IMPL").unwrap_or_default();
    match raw.trim().to_ascii_lowercase().as_str() {
        "python" => "python",
        _ => "rust",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Process-wide lock serialising the three `impl_for` tests below.
    ///
    /// All three mutate the single process-global
    /// `WYLDE_HARNESS_MEMORY_IMPL` env var. Per-test snapshot+restore is
    /// *not* enough under cargo's default parallel runner: env vars are
    /// shared across the whole process, so one test's `remove_var` /
    /// `set_var` can clobber another between *its* set and *its* assert.
    /// That race made `impl_for_honours_python_rollback` flaky — green in
    /// isolation, intermittently red in the full suite (e.g. the defaults
    /// test removing the var after the rollback test set it to `python`
    /// but before it asserted). Holding this mutex for the duration of
    /// each test means only one runs at a time, so the snapshot/restore
    /// is observed atomically.
    ///
    /// Local `Mutex` rather than the `serial_test` crate to match the
    /// crate's existing idiom (`tooling::consent::serial_test_guard`) and
    /// avoid a new dependency for three tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard: holds [`ENV_LOCK`] (serialising env-mutating tests),
    /// snapshots `WYLDE_HARNESS_MEMORY_IMPL` on construction, and restores
    /// it on drop — including on a panicking failed assertion, so a
    /// failure can't leak the var into the next test.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            // Recover from a poisoned lock (a prior test panicked while
            // holding it) instead of cascade-failing — the env is
            // restored on drop regardless.
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("WYLDE_HARNESS_MEMORY_IMPL").ok(); // wylde-check: discard-result-ok
            Self { _lock: lock, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("WYLDE_HARNESS_MEMORY_IMPL", v),
                None => std::env::remove_var("WYLDE_HARNESS_MEMORY_IMPL"),
            }
        }
    }

    #[test]
    fn impl_for_defaults_to_rust_when_env_unset() {
        let _env = EnvGuard::acquire();
        std::env::remove_var("WYLDE_HARNESS_MEMORY_IMPL");
        assert_eq!(impl_for(), "rust");
    }

    #[test]
    fn impl_for_clamps_unknown_values_to_rust() {
        let _env = EnvGuard::acquire();
        std::env::set_var("WYLDE_HARNESS_MEMORY_IMPL", "javascript");
        assert_eq!(impl_for(), "rust");
    }

    #[test]
    fn impl_for_honours_python_rollback() {
        let _env = EnvGuard::acquire();
        std::env::set_var("WYLDE_HARNESS_MEMORY_IMPL", "python");
        assert_eq!(impl_for(), "python");
    }
}
