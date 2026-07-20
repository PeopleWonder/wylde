//! Budgeted, fail-soft embedding for the memory write path.
//!
//! The authoritative JSON stores (`long_term/entries.rs`,
//! `workspace/store.rs`) are deliberately kept free of an `wylde-ollama`
//! dependency — their `save` / `update` functions take an *optional*
//! precomputed vector. Historically nothing populated that vector unless
//! the caller pre-embedded, so the `long_term.vec.bin` mirror drifted
//! empty and semantic search silently degraded to recency / text
//! overlap.
//!
//! This module is the one place the write surface reaches the embedder.
//! Handlers (`tools::memory`, `workspace::actions`) and the reflection
//! cycles call [`embed_for_write`] to turn a body into a vector *at the
//! async boundary*, then hand that vector to the sync store. It reuses
//! the project's brokered embedding path
//! ([`crate::memory::embeddings::embed_one`] → `ollama.embed` IPC →
//! VRAM-broker → nomic-embed-text); it never talks to Ollama directly.
//!
//! ## Budget + fail-soft
//!
//! Embedding is bounded by [`embed_write_budget`] (default 1.2s, the
//! same envelope the turn gather uses; override with
//! `WYLDE_EMBED_WRITE_BUDGET_MS`). Any failure — embedder down, over
//! budget, empty text — returns `None`, and the caller saves the record
//! JSON-only exactly as before. A write is never blocked or failed by a
//! slow / absent embedder.
//!
//! **That record does NOT catch up on its own.** This comment used to claim
//! the mirror caught up "on the next write (or a `reindex`)"; neither was
//! true. A later write embeds only the record being written, and there was no
//! `reindex` at all — so a record saved while the embedder was down stayed
//! absent from the mirror permanently, and semantic search silently skipped
//! it. Closing that gap now requires an explicit rebuild:
//! [`crate::memory::long_term::reindex_vectors`] or
//! [`crate::memory::workspace::store::reindex_vectors`] (#136).

use std::time::Duration;

use crate::memory::embeddings;

/// Default embed budget for a single write. Matches the gather's 1.2s
/// envelope so a save never stalls longer than a prompt assembly would.
pub const DEFAULT_EMBED_WRITE_BUDGET_MS: u64 = 1200;

/// Effective per-write embed budget. `WYLDE_EMBED_WRITE_BUDGET_MS`
/// overrides the [`DEFAULT_EMBED_WRITE_BUDGET_MS`] default (tests pin it
/// low so the no-embedder path returns promptly).
pub fn embed_write_budget() -> Duration {
    let ms = std::env::var("WYLDE_EMBED_WRITE_BUDGET_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_EMBED_WRITE_BUDGET_MS);
    Duration::from_millis(ms)
}

/// Embed `text` for a write, bounded by [`embed_write_budget`]. Returns
/// `None` (never an error) when the text is blank, the embedder is
/// unreachable, or the call exceeds the budget — the caller then saves
/// JSON-only. Reuses the brokered [`embeddings::embed_one`] path.
pub async fn embed_for_write(text: &str) -> Option<Vec<f32>> {
    if text.trim().is_empty() {
        return None;
    }
    match tokio::time::timeout(embed_write_budget(), embeddings::embed_one(text.to_owned())).await {
        Ok(Ok(vector)) => Some(vector),
        Ok(Err(e)) => {
            tracing::debug!("embed_write: embed failed, saving without vector: {e}");
            None
        }
        Err(_) => {
            tracing::debug!("embed_write: embed exceeded budget, saving without vector");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_reads_env_override() {
        let _g = crate::memory::common::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("WYLDE_EMBED_WRITE_BUDGET_MS");
        std::env::set_var("WYLDE_EMBED_WRITE_BUDGET_MS", "37");
        assert_eq!(embed_write_budget(), Duration::from_millis(37));
        std::env::remove_var("WYLDE_EMBED_WRITE_BUDGET_MS");
        assert_eq!(
            embed_write_budget(),
            Duration::from_millis(DEFAULT_EMBED_WRITE_BUDGET_MS)
        );
        match prior {
            Some(v) => std::env::set_var("WYLDE_EMBED_WRITE_BUDGET_MS", v),
            None => std::env::remove_var("WYLDE_EMBED_WRITE_BUDGET_MS"),
        }
    }

    #[tokio::test]
    async fn blank_text_returns_none_without_touching_backend() {
        assert!(embed_for_write("   \t\n").await.is_none());
    }
}
