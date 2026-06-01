//! Text-input search over long-term memory.
//!
//! Rust port of the `query: str` half of `Core/harness/memory/long_term.py::search`.
//! Embeds the query via [`crate::memory::embeddings::embed_one`], then
//! re-uses [`crate::memory::long_term::search`] for vector retrieval +
//! importance/recency re-ranking. Kept in its own file so the rest of
//! `long_term/` stays free of an embedder dependency (the precomputed
//! [`search`] path still works without it).
//!
//! The Python `search` function swallows the embed exception and returns
//! `[]`; the Rust port surfaces it so the caller can distinguish "no
//! results" from "embedder is down". The IPC layer above will translate
//! that into a structured wire reply.

use super::{search, SearchHit, DEFAULT_DECAY_DAYS};
use crate::memory::embeddings::{self, EmbedError};

#[derive(Debug, thiserror::Error)]
pub enum TextSearchError {
    /// Query was empty / whitespace-only — caller should short-circuit
    /// rather than treat this as a real backend failure.
    #[error("query is empty")]
    EmptyQuery,
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

/// Embed the query, then rank long-term records by similarity +
/// importance + recency decay. Empty / whitespace-only queries return
/// `EmptyQuery` (the Python impl silently returns `[]`; we surface it
/// so callers can choose to log or fold to an empty list).
///
/// `decay_days = None` defaults to [`DEFAULT_DECAY_DAYS`].
pub async fn text_search(
    query: &str,
    limit: usize,
    decay_days: Option<f64>,
) -> Result<Vec<SearchHit>, TextSearchError> {
    if query.trim().is_empty() {
        return Err(TextSearchError::EmptyQuery);
    }
    let vec = embeddings::embed_one(query.to_owned()).await?;
    let decay = decay_days.unwrap_or(DEFAULT_DECAY_DAYS);
    Ok(search(vec, limit, Some(decay)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_search_rejects_empty_query() {
        let err = text_search("", 5, None).await.unwrap_err();
        assert!(matches!(err, TextSearchError::EmptyQuery));
    }

    #[tokio::test]
    async fn text_search_rejects_whitespace_only_query() {
        let err = text_search("   \t\n", 5, None).await.unwrap_err();
        assert!(matches!(err, TextSearchError::EmptyQuery));
    }

    // Non-empty path requires a running wylde-ollama pipe — covered by
    // the ignore-marked live integration test under tests/embed_live.rs.
}
