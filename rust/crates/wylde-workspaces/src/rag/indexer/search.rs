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

/// Absolute cosine noise floor for the dynamic-k cutoff. A query whose
/// *best* hit scores below this retrieves nothing — the workspace index
/// holds nothing on-topic, so injecting its strongest-but-still-weak chunks
/// only pads the prompt with noise.
///
/// **Empirically calibrated against the live index** (14k chunks,
/// `nomic-embed-text`, no task-prefixing → anisotropic cosines with a high
/// baseline). Measured top-1 scores cleanly separate by relevance: genuinely
/// off-topic queries (pizza recipe, dog training, weather) top out at
/// ~0.49–0.51, while on-topic queries — even a vague "why did that happen?"
/// — sit at ~0.60–0.69. `0.55` lands in that gap, so off-topic queries
/// inject nothing while on-topic queries keep their hits. (A query the model
/// itself can't tell from on-topic — e.g. song lyrics scoring ~0.59 — is a
/// limit of the embedding, not of this cutoff.) Re-measure if the embedding
/// model or task-prefixing changes.
const MIN_ABSOLUTE_SCORE: f64 = 0.55;

/// Relative dominance floor for the dynamic-k cutoff: a hit is only worth a
/// prompt slot if its cosine is at least this fraction of the *top* hit's.
/// When one result dominates (a sharp cliff after rank 1) the weaker tail is
/// trimmed instead of padding the slot; when several hits cluster near the
/// top they all clear it and the full budget is used.
const RELATIVE_FLOOR: f64 = 0.6;

/// Ranking core: score every chunk by cosine against `query_vec`, then
/// select `k` with Maximal Marginal Relevance so near-duplicate chunks
/// don't crowd out the prompt's RAG slot. Split out for direct unit testing
/// without a live embedder.
///
/// The pure-cosine scoring is unchanged — MMR only governs *selection*
/// among the top [`MMR_POOL`] candidates, and each returned hit's `score`
/// is still its true cosine relevance.
///
/// `k` is the *budget* (max slots), not a fixed count: a [`dynamic_k`] cutoff
/// trims weak/dominated hits first, so an off-topic query returns few or no
/// chunks instead of padding the slot up to `k`.
pub fn rank(query_vec: &[f32], chunks: Vec<IndexedChunk>, k: usize) -> Vec<SearchHit> {
    if k == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(f64, IndexedChunk)> = chunks
        .into_iter()
        .map(|c| (cosine(query_vec, &c.vector), c))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    // Vary how many slots are actually warranted by the score distribution
    // before spending the MMR/diversity budget. Nothing clears the cutoff →
    // inject nothing (the off-topic case).
    let keep = dynamic_k(&scored, k);
    if keep == 0 {
        return Vec::new();
    }
    // Over-fetch a pool of the strongest cosine hits, then MMR-select down to
    // the warranted count. The pool floor of `keep` keeps behaviour intact
    // when `keep` exceeds it.
    scored.truncate(MMR_POOL.max(keep));
    mmr_select(scored, keep)
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

/// Decide how many of the top hits are *worth* a prompt slot, given the
/// budget `k` and the descending-cosine `scored` candidates. Returns a count
/// in `0..=k`:
///
/// * `0` — the best hit is below [`MIN_ABSOLUTE_SCORE`]: nothing on-topic, so
///   inject nothing rather than padding with noise.
/// * `1` — one result dominates (the rest fall below [`RELATIVE_FLOOR`]·top):
///   don't dilute it with weak tail hits.
/// * up to `k` — several hits cluster near the top: use the full budget.
///
/// Because `scored` is sorted descending, the kept hits are the contiguous
/// prefix that clears `max(MIN_ABSOLUTE_SCORE, RELATIVE_FLOOR·top)`.
fn dynamic_k(scored: &[(f64, IndexedChunk)], k: usize) -> usize {
    if k == 0 || scored.is_empty() {
        return 0;
    }
    let top = scored[0].0;
    // Best hit is noise → retrieve nothing.
    if top < MIN_ABSOLUTE_SCORE {
        return 0;
    }
    let threshold = MIN_ABSOLUTE_SCORE.max(RELATIVE_FLOOR * top);
    let kept = scored
        .iter()
        .take(k)
        .take_while(|(score, _)| *score >= threshold)
        .count();
    // `top` cleared `threshold` by construction, so at least the dominant
    // hit is always kept once we get here.
    kept.max(1)
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

    /// Build descending-cosine `(score, chunk)` pairs for direct
    /// [`dynamic_k`] boundary tests, without an embedder.
    fn scored(scores: &[f64]) -> Vec<(f64, IndexedChunk)> {
        scores
            .iter()
            .enumerate()
            .map(|(i, &s)| (s, chunk(&format!("/c{i}.md"), vec![1.0, 0.0], "c")))
            .collect()
    }

    /// The same cutoff threshold `dynamic_k` applies for a given top score —
    /// so these tests pin the *logic*, not the calibrated constants, and stay
    /// green if `MIN_ABSOLUTE_SCORE` / `RELATIVE_FLOOR` are re-tuned.
    fn threshold_for(top: f64) -> f64 {
        MIN_ABSOLUTE_SCORE.max(RELATIVE_FLOOR * top)
    }

    #[test]
    fn dynamic_k_zero_when_best_hit_is_noise() {
        // Top hit below the absolute floor → nothing on-topic, inject none.
        let noise = MIN_ABSOLUTE_SCORE - 0.05;
        assert_eq!(dynamic_k(&scored(&[noise, noise - 0.05, noise - 0.1]), 5), 0);
    }

    #[test]
    fn dynamic_k_one_when_top_dominates_and_tail_is_below_floor() {
        // A strong top hit, with the tail below the cutoff → only the
        // dominant hit is kept (don't dilute it with weak padding).
        let top = 0.9_f64;
        let thr = threshold_for(top);
        assert_eq!(dynamic_k(&scored(&[top, thr - 0.05, thr - 0.1]), 5), 1);
    }

    #[test]
    fn dynamic_k_keeps_cluster_above_the_floor() {
        // Three hits all comfortably above the cutoff → full count kept.
        let top = 0.9_f64;
        let thr = threshold_for(top);
        assert_eq!(dynamic_k(&scored(&[top, thr + 0.05, thr + 0.02]), 5), 3);
    }

    #[test]
    fn dynamic_k_capped_by_budget() {
        let top = 0.9_f64;
        assert_eq!(dynamic_k(&scored(&[top, top, top, top]), 2), 2);
    }

    #[test]
    fn dynamic_k_relative_floor_trims_when_top_is_very_high() {
        // When `RELATIVE_FLOOR·top` exceeds the absolute floor, a tail hit
        // that clears the absolute floor but is dominated by a very strong
        // top is still trimmed — the dominance branch.
        let top = 0.99_f64;
        let rel = RELATIVE_FLOOR * top;
        // Only meaningful while the relative floor is the binding one.
        if rel > MIN_ABSOLUTE_SCORE {
            let tail = (rel + MIN_ABSOLUTE_SCORE) / 2.0; // above absolute, below relative
            assert_eq!(dynamic_k(&scored(&[top, tail]), 5), 1);
        }
    }

    #[test]
    fn dynamic_k_zero_budget_or_empty() {
        assert_eq!(dynamic_k(&scored(&[0.9]), 0), 0);
        assert_eq!(dynamic_k(&[], 5), 0);
    }

    /// Unit vector whose cosine against the `[1, 0]` query equals `c`.
    fn vec_with_cosine(c: f32) -> Vec<f32> {
        vec![c, (1.0 - c * c).max(0.0).sqrt()]
    }

    #[test]
    fn rank_dynamic_k_trims_weak_tail_when_top_dominates() {
        // Budget of 5, but only the strong hit warrants a slot: the two weak
        // tail hits fall below the cutoff and are dropped, not padded in.
        let top = 0.9_f64;
        let weak = (threshold_for(top) - 0.1) as f32;
        let query = vec![1.0_f32, 0.0];
        let chunks = vec![
            chunk("/strong.md", vec_with_cosine(top as f32), "strong"),
            chunk("/weak-a.md", vec_with_cosine(weak), "weak a"),
            chunk("/weak-b.md", vec_with_cosine(weak), "weak b"),
        ];
        let hits = rank(&query, chunks, 5);
        assert_eq!(hits.len(), 1, "weak tail trimmed, not padded to budget");
        assert_eq!(hits[0].file_path, "/strong.md");
    }

    #[test]
    fn rank_dynamic_k_empty_for_off_topic_query() {
        // Every hit is below the absolute floor: an off-topic query injects
        // nothing instead of padding the slot to k.
        let noise = (MIN_ABSOLUTE_SCORE - 0.1) as f32;
        let query = vec![1.0_f32, 0.0];
        let chunks = vec![
            chunk("/a.md", vec_with_cosine(noise), "a"),
            chunk("/b.md", vec_with_cosine(noise - 0.05), "b"),
            chunk("/c.md", vec_with_cosine(noise - 0.1), "c"),
        ];
        assert!(rank(&query, chunks, 5).is_empty());
    }

    #[test]
    fn rank_dynamic_k_uses_budget_when_hits_cluster() {
        // Three hits clustered above the floor all warrant a slot → all three
        // returned (the full available budget), none trimmed.
        let top = 0.9_f64;
        let thr = threshold_for(top);
        let query = vec![1.0_f32, 0.0];
        let chunks = vec![
            chunk("/a.md", vec_with_cosine(top as f32), "a"),
            chunk("/b.md", vec_with_cosine((thr + 0.08) as f32), "b"),
            chunk("/c.md", vec_with_cosine((thr + 0.04) as f32), "c"),
        ];
        let hits = rank(&query, chunks, 5);
        assert_eq!(hits.len(), 3, "clustered hits all warrant a slot");
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
