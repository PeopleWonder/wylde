//! Vector search over a workspace's file index.
//!
//! Embeds the query once via the shared `nomic-embed-text` embedder
//! (`crate::embeddings`), then does a brute-force cosine scan over
//! the persisted chunks — see `store.rs` for why brute-force, not ANN.
//!
//! **Never errors.** A missing index, an empty query, or an unreachable
//! embedder all yield an empty result, so the pointer-only fallback holds:
//! `rag_query` returns `[]`, never an error.

use serde_json::{json, Value};

use super::store::{self, IndexedChunk};
use crate::rag::cosine;

/// One ranked search hit. Shape mirrors the retired Python verb:
/// `{file_path, line_range, content, score}`.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    /// Absolute source-file path.
    pub file_path: String,
    /// `[start_line, end_line]`, 1-based inclusive.
    pub line_range: [u32; 2],
    /// The chunk text.
    pub content: String,
    /// Cosine similarity in `[-1, 1]` (higher = closer).
    pub score: f64,
    /// 0-based chunk index within its file (disambiguates same-file hits).
    pub chunk_idx: u32,
}

impl SearchHit {
    /// JSON shape handed to the IPC layer / GUI.
    pub fn to_value(&self) -> Value {
        json!({
            "file_path": self.file_path,
            "line_range": [self.line_range[0], self.line_range[1]],
            "content": self.content,
            "score": self.score,
            "chunk_idx": self.chunk_idx,
        })
    }
}

/// Top-`k` chunks for `query` within `workspace_id`, highest score first.
///
/// Returns an empty vec when the workspace has no index, the query is
/// blank, or the embedder is unreachable — the caller treats `[]` as "no
/// snippets", never an error.
pub async fn query(workspace_id: &str, query_text: &str, k: usize) -> Vec<SearchHit> {
    if query_text.trim().is_empty() || k == 0 {
        return Vec::new();
    }
    let chunks = store::load_chunks(workspace_id);
    if chunks.is_empty() {
        return Vec::new();
    }
    let query_vec = match crate::embeddings::embed_one(query_text.to_owned()).await {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => return Vec::new(),
        Err(e) => {
            tracing::warn!("workspaces.rag: query embed failed for {workspace_id}: {e}");
            return Vec::new();
        }
    };
    rank(&query_vec, chunks, k)
}

/// MMR relevance/diversity trade-off (the `λ` in the standard formula):
/// `λ·rel − (1−λ)·max_sim_to_selected`. At `0.7` query-relevance stays
/// dominant while near-duplicate chunks are still penalised out of the top-k.
const MMR_LAMBDA: f64 = 0.7;

/// Over-fetch depth: how many of the strongest cosine hits MMR considers
/// before selecting the final `k`. Larger gives MMR more redundant
/// neighbours to prune; capped to keep the O(pool·k) similarity work cheap
/// on large indexes. (`pool` is always at least `k`.)
const MMR_POOL: usize = 20;

/// Ranking core: score every chunk by cosine against `query_vec`, then
/// select `k` with Maximal Marginal Relevance so near-duplicate chunks
/// don't crowd out the prompt's RAG slot. Split out for direct unit testing
/// without a live embedder.
///
/// The pure-cosine scoring is unchanged — MMR only governs *selection*
/// among the top [`MMR_POOL`] candidates, and each returned hit's `score`
/// is still its true cosine relevance.
pub fn rank(query_vec: &[f32], chunks: Vec<IndexedChunk>, k: usize) -> Vec<SearchHit> {
    if k == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(f64, IndexedChunk)> = chunks
        .into_iter()
        .map(|c| (cosine(query_vec, &c.vector), c))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    // Over-fetch a pool of the strongest cosine hits, then MMR-select down
    // to k. The pool floor of k keeps behaviour intact when k exceeds it.
    scored.truncate(MMR_POOL.max(k));
    mmr_select(scored, k)
        .into_iter()
        .map(|(score, c)| SearchHit {
            file_path: c.path,
            line_range: [c.start_line, c.end_line],
            content: c.content,
            score,
            chunk_idx: c.chunk_idx,
        })
        .collect()
}

/// Greedy Maximal Marginal Relevance selection over `candidates`
/// (pre-sorted by descending cosine relevance). Picks the highest-relevance
/// chunk first, then repeatedly the chunk maximising
/// `λ·rel − (1−λ)·max cosine to anything already picked`, so a chunk nearly
/// identical to one already chosen is demoted in favour of a fresh-but-still-
/// relevant one. Returns at most `k` items in selection order.
fn mmr_select(
    mut candidates: Vec<(f64, IndexedChunk)>,
    k: usize,
) -> Vec<(f64, IndexedChunk)> {
    let target = k.min(candidates.len());
    if target == 0 {
        return Vec::new();
    }
    let mut selected: Vec<(f64, IndexedChunk)> = Vec::with_capacity(target);
    // Seed with the top cosine hit — candidates is already sorted descending.
    selected.push(candidates.remove(0));
    while selected.len() < target && !candidates.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f64::NEG_INFINITY;
        for (i, (rel, cand)) in candidates.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|(_, s)| cosine(&cand.vector, &s.vector))
                .fold(0.0_f64, f64::max);
            let mmr = MMR_LAMBDA * rel - (1.0 - MMR_LAMBDA) * max_sim;
            if mmr > best_mmr {
                best_mmr = mmr;
                best_idx = i;
            }
        }
        selected.push(candidates.remove(best_idx));
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(path: &str, vector: Vec<f32>, content: &str) -> IndexedChunk {
        IndexedChunk {
            id: format!("{path}-0"),
            path: path.to_owned(),
            chunk_idx: 0,
            content: content.to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 4,
            vector,
        }
    }

    #[test]
    fn rank_orders_by_cosine_and_truncates_to_k() {
        let query = vec![1.0_f32, 0.0, 0.0];
        let chunks = vec![
            chunk("/far.md", vec![0.0, 1.0, 0.0], "far"), // orthogonal → 0
            chunk("/near.md", vec![0.9, 0.1, 0.0], "near"), // close → high
            chunk("/mid.md", vec![0.6, 0.6, 0.0], "mid"), // middling
        ];
        let hits = rank(&query, chunks, 2);
        assert_eq!(hits.len(), 2, "truncated to k");
        assert_eq!(hits[0].file_path, "/near.md", "nearest first");
        assert_eq!(hits[1].file_path, "/mid.md");
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[0].line_range, [1, 4]);
    }

    #[test]
    fn rank_empty_chunks_is_empty() {
        assert!(rank(&[1.0, 0.0], Vec::new(), 5).is_empty());
    }

    #[test]
    fn rank_k_zero_is_empty() {
        let chunks = vec![chunk("/a.md", vec![1.0, 0.0], "a")];
        assert!(rank(&[1.0, 0.0], chunks, 0).is_empty());
    }

    #[test]
    fn rank_mmr_drops_near_duplicate_for_diverse_chunk() {
        // Query along the first axis. Two chunks are identical (a perfect
        // near-duplicate pair) and a third is *equally* relevant to the query
        // but points in a different residual direction. Pure top-k cosine
        // would return both duplicates; MMR must swap the second duplicate
        // out for the distinct chunk.
        let query = vec![1.0_f32, 0.0, 0.0, 0.0];
        let chunks = vec![
            chunk("/a.md", vec![0.8, 0.6, 0.0, 0.0], "first"),
            chunk("/a-dup.md", vec![0.8, 0.6, 0.0, 0.0], "near-duplicate of first"),
            chunk("/b.md", vec![0.8, 0.0, 0.6, 0.0], "equally relevant but distinct"),
        ];
        let hits = rank(&query, chunks, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].file_path, "/a.md", "top relevance kept first");
        assert_eq!(
            hits[1].file_path, "/b.md",
            "near-duplicate demoted; diverse chunk selected"
        );
        // Returned score is still the true cosine relevance (all three ≈0.8;
        // tolerance accounts for the f32 vector components).
        assert!((hits[0].score - 0.8).abs() < 1e-6);
    }

    #[test]
    fn rank_mmr_keeps_relevance_dominant() {
        // A clearly irrelevant chunk must never beat a relevant one on
        // diversity alone — λ=0.7 keeps relevance dominant.
        let query = vec![1.0_f32, 0.0, 0.0];
        let chunks = vec![
            chunk("/near.md", vec![0.9, 0.1, 0.0], "near"),
            chunk("/mid.md", vec![0.6, 0.6, 0.0], "mid"),
            chunk("/orthogonal.md", vec![0.0, 1.0, 0.0], "irrelevant"),
        ];
        let hits = rank(&query, chunks, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].file_path, "/near.md");
        assert_eq!(hits[1].file_path, "/mid.md", "relevant mid beats orthogonal");
    }

    #[test]
    fn to_value_has_the_python_shape() {
        let hit = SearchHit {
            file_path: "/a.md".into(),
            line_range: [3, 9],
            content: "body".into(),
            score: 0.42,
            chunk_idx: 2,
        };
        let v = hit.to_value();
        assert_eq!(v["file_path"], "/a.md");
        assert_eq!(v["line_range"], json!([3, 9]));
        assert_eq!(v["content"], "body");
        assert_eq!(v["score"], 0.42);
    }
}
