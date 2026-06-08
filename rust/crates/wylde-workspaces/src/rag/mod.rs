//! `rag/` — RAG integration for a workspace's folder.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Rag/`.
//!
//! A workspace *is* a folder (per memory `wylde_rag_workspaces`). This
//! module translates that folder into a RAG query scope so retrieval for
//! a turn anchored to the workspace is bounded to the workspace's files,
//! rather than searching the global index.
//!
//! This is the read/scope side only. The heavy indexing machinery
//! (LanceDB, the embedder) is out of scope for the redesign scaffold and
//! continues to live where it does today (see the design doc's
//! migration section).
//!
//! ## Split
//!
//! * [`scope`] — folder → [`WorkspaceRagScope`] translation + the
//!   retrieval entrypoint the prompt builder calls.
//! * [`indexer`] — the file-RAG index itself: walk → chunk → embed →
//!   store + k-NN search + delta-reindex. This is the Rust port of the
//!   retired Python/LanceDB indexer the redesign scaffold deferred; it
//!   slots in behind [`scope::retrieve`] without changing the
//!   prompt-builder contract.

pub mod indexer;
pub mod scope;

pub use scope::WorkspaceRagScope;

/// Cosine similarity in `[-1, 1]` between two equal-length vectors; `0.0`
/// for empty / mismatched / zero-norm inputs.
///
/// Relocated alongside the indexer (Slice 0b) — the search ranker
/// ([`indexer::search::rank`]) is the only consumer. The harness keeps its
/// own copy in `memory::query` for the workspace-notes tier (moves in 0c).
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod cosine_tests {
    use super::cosine;

    #[test]
    fn cosine_basic() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}

