//! Fetch + score workspace-notes entries for prompt injection and search.
//!
//! Relocated from the harness `workspaces::memory::query` (Slice 0c). The
//! prompt builder asks [`top_entries`] for the top-K notes to inject as the
//! workspace-memory slot; the `workspaces.notes.search` verb reuses the same
//! ranking with the search query in place of the turn's user message.
//!
//! ## Scoring (Q3 — recency + relevance, α = 0.4 / 0.6)
//!
//! Per turn we embed the query **once** (reusing the existing
//! `nomic-embed-text` embedder — no new model load) and score each entry by
//!
//! ```text
//! score = α · recency + (1 − α) · cosine(query, entry)
//! ```
//!
//! with **α = [`ALPHA_RECENCY`] = 0.4** (so relevance carries 0.6).
//! `recency` is an exponential decay of the entry's `last_used_at` into
//! `[0, 1]`; `cosine` is the dot product of the (L2-normalized at write
//! time) embeddings, treated as 0 when either side has no embedding.
//!
//! **Graceful degradation:** if the embedder is unreachable (Ollama down) or
//! the query is empty, the relevance term is 0 and the blend collapses to
//! pure recency — the slot still works, just without semantic ranking.

use super::entry::{self, WorkspaceMemoryEntry};

/// Recency's share of the blended score (Q3). Relevance gets the rest.
pub const ALPHA_RECENCY: f64 = 0.4;

/// Half-life-ish decay horizon (days) for the recency term.
pub const RECENCY_DECAY_DAYS: f64 = 30.0;

const SECONDS_PER_DAY: f64 = 86_400.0;

/// A request for the most relevant workspace-notes entries to inject.
#[derive(Clone, Debug)]
pub struct WorkspaceMemoryQuery {
    /// Which workspace's bucket to read.
    pub workspace_id: String,

    /// The query text — embedded once for relevance scoring. May be empty
    /// for a pure-recency fetch.
    pub user_message: String,

    /// Max entries to return.
    pub limit: usize,
}

impl WorkspaceMemoryQuery {
    /// Default injection budget for the workspace-memory slot.
    pub const DEFAULT_LIMIT: usize = 5;
}

/// Exponential recency score in `[0, 1]`: 1.0 for "just used", decaying
/// with age over [`RECENCY_DECAY_DAYS`].
pub fn recency_score(last_used_at: f64, now: f64) -> f64 {
    let age_days = ((now - last_used_at).max(0.0)) / SECONDS_PER_DAY;
    (-age_days / RECENCY_DECAY_DAYS.max(1e-6)).exp()
}

/// Cosine similarity of two embedding vectors. Returns 0.0 when either
/// is empty or the lengths differ (so a missing embedding contributes no
/// relevance rather than corrupting the rank).
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

/// Blend recency and relevance per Q3: `α·recency + (1−α)·cosine`.
pub fn blended_score(recency: f64, cosine: f64, alpha: f64) -> f64 {
    alpha * recency + (1.0 - alpha) * cosine
}

/// Embed `text` with the shared `nomic-embed-text` embedder, returning
/// an empty vector on any failure (so callers degrade to recency-only).
pub async fn embed_text(text: &str) -> Vec<f32> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    crate::embeddings::embed_one(text.to_owned())
        .await
        .unwrap_or_default()
}

/// Default time budget for an embed on a write verb. The `workspaces.notes.*`
/// write verbs are Medium-tier (2s client budget); the embedder's own retry
/// ladder is ~3.5s when the backend is unreachable, which would blow that
/// budget. Bound it well under 2s so a down/slow embedder degrades the note
/// to recency-only (empty embedding) instead of timing the verb out.
pub const EMBED_WRITE_BUDGET: std::time::Duration = std::time::Duration::from_millis(1200);

/// Like [`embed_text`] but abandons the embed (returning an empty vector) if
/// it doesn't complete within `budget`. Keeps the embed-on-write path inside
/// the verb's timeout tier — a missing embedding just costs relevance
/// ranking, never the write itself.
pub async fn embed_text_bounded(text: &str, budget: std::time::Duration) -> Vec<f32> {
    // On timeout (`Elapsed`) the embed is abandoned → empty vector.
    tokio::time::timeout(budget, embed_text(text))
        .await
        .unwrap_or_default()
}

/// Rank loaded `entries` against a pre-computed `query_vec`, highest-scoring
/// first, truncated to `limit`. Pure (no I/O) — separated from the embed so
/// callers choose their own embed policy (bounded for the search verb,
/// unbounded for the in-process prompt builder).
pub fn rank_entries(
    entries: Vec<WorkspaceMemoryEntry>,
    query_vec: &[f32],
    limit: usize,
) -> Vec<WorkspaceMemoryEntry> {
    let now = crate::registry::epoch_now();
    let mut scored: Vec<(f64, WorkspaceMemoryEntry)> = entries
        .into_iter()
        .map(|e| {
            let recency = recency_score(e.last_used_at, now);
            let relevance = cosine(query_vec, &e.embedding);
            (blended_score(recency, relevance, ALPHA_RECENCY), e)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(limit.max(1))
        .map(|(_, e)| e)
        .collect()
}

/// Return the top entries to inject/return for `query`, highest-scoring
/// first. Embeds the query once (unbounded — the in-process prompt builder
/// has no IPC deadline), blends recency + relevance, truncates to
/// `query.limit`.
pub async fn top_entries(query: &WorkspaceMemoryQuery) -> Vec<WorkspaceMemoryEntry> {
    let entries = entry::load(&query.workspace_id);
    if entries.is_empty() {
        return entries;
    }
    let query_vec = embed_text(&query.user_message).await;
    rank_entries(entries, &query_vec, query.limit)
}

/// Like [`top_entries`] but bounds the query embed to `budget` so it fits a
/// verb's timeout tier (the `workspaces.notes.search` path). A slow/down
/// embedder degrades to pure-recency ranking rather than timing out.
pub async fn top_entries_bounded(
    query: &WorkspaceMemoryQuery,
    budget: std::time::Duration,
) -> Vec<WorkspaceMemoryEntry> {
    let entries = entry::load(&query.workspace_id);
    if entries.is_empty() {
        return entries;
    }
    let query_vec = embed_text_bounded(&query.user_message, budget).await;
    rank_entries(entries, &query_vec, query.limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recency_score_is_one_for_now_and_decays() {
        let now = 1_000_000.0;
        assert!((recency_score(now, now) - 1.0).abs() < 1e-9);
        let older = recency_score(now - 30.0 * SECONDS_PER_DAY, now);
        // One decay horizon → ~1/e.
        assert!((older - std::f64::consts::E.recip()).abs() < 1e-6, "got {older}");
        // Future timestamps clamp to 1.0 (age floored at 0).
        assert!((recency_score(now + 100.0, now) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_handles_identical_orthogonal_and_missing() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        // Missing / mismatched → 0, never a panic.
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn blended_score_weights_match_locked_alpha() {
        assert!((ALPHA_RECENCY - 0.4).abs() < 1e-12);
        // Pure-recency entry (no relevance) vs pure-relevance entry.
        let recency_only = blended_score(1.0, 0.0, ALPHA_RECENCY);
        let relevance_only = blended_score(0.0, 1.0, ALPHA_RECENCY);
        assert!((recency_only - 0.4).abs() < 1e-12);
        assert!((relevance_only - 0.6).abs() < 1e-12);
        // Relevance outweighs recency at equal magnitude (0.6 > 0.4).
        assert!(relevance_only > recency_only);
    }

    #[test]
    fn blended_ranking_prefers_relevant_then_recent() {
        // A highly-relevant-but-old entry should beat a recent-but-
        // irrelevant one under α = 0.4 / 0.6.
        let old_relevant = blended_score(0.2, 0.95, ALPHA_RECENCY);
        let recent_irrelevant = blended_score(1.0, 0.05, ALPHA_RECENCY);
        assert!(old_relevant > recent_irrelevant, "{old_relevant} vs {recent_irrelevant}");
    }
}
