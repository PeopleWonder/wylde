//! Reciprocal Rank Fusion (RRF) of the dense (cosine) and lexical (BM25) arms
//! (lexical-bm25 plan L4 / §1.3) — **pure**, no IO, fully unit-testable.
//!
//! Each arm contributes `weight / (rrf_k + rank)` to a chunk's fused score,
//! where `rank` is the chunk's 0-based position in that arm's descending order.
//! A chunk missing from an arm contributes `0` from it. RRF scores are tiny
//! (`≤ 1/rrf_k` per arm) and **scale-free**, which is exactly why the relative
//! dynamic-k floor (`keep ≥ floor · top`) transfers cleanly from the dense path
//! while the absolute cosine floor does not (§1.4 — handled in `search.rs`).
//!
//! This module knows nothing about chunks, tantivy, or cosine math — it fuses
//! two rank lists over a shared index space `0..n`, so the same code is exercised
//! by deterministic unit tests and by the live retriever identically.

use crate::rag::LexicalConfig;

/// One arm's RRF contribution for a hit at 0-based `rank`: `weight / (k + rank)`.
/// Monotonically decreasing in `rank`, bounded by `weight / k`.
fn rrf_contribution(weight: f64, rrf_k: f64, rank: usize) -> f64 {
    weight / (rrf_k + rank as f64)
}

/// Fuse a dense ranking and a lexical ranking over a shared `0..n` index space.
///
/// * `n` — the number of candidates (chunks).
/// * `dense_order` — candidate indices in **descending cosine** order
///   (`dense_order[r]` is the index ranked `r`). Normally a permutation of
///   `0..n`; any index it omits simply gets no dense contribution.
/// * `lex_hits` — `(index, bm25)` pairs in **descending BM25** order (the lexical
///   arm's output, already a subset — only matched candidates).
/// * `cfg` — supplies `rrf_k`, `w_dense`, `w_lex`.
///
/// Returns a vector parallel to `0..n`: `(fused_score, lexical_bm25_opt)`. The
/// `lexical` half is `Some(bm25)` for a chunk the lexical arm matched (so a
/// lexical-only hit isn't mistaken for a weak one), else `None`.
pub fn fuse(
    n: usize,
    dense_order: &[usize],
    lex_hits: &[(usize, f64)],
    cfg: &LexicalConfig,
) -> Vec<(f64, Option<f64>)> {
    let mut out = vec![(0.0_f64, None); n];
    for (rank, &idx) in dense_order.iter().enumerate() {
        if let Some(slot) = out.get_mut(idx) {
            slot.0 += rrf_contribution(cfg.w_dense, cfg.rrf_k, rank);
        }
    }
    for (rank, &(idx, bm25)) in lex_hits.iter().enumerate() {
        if let Some(slot) = out.get_mut(idx) {
            slot.0 += rrf_contribution(cfg.w_lex, cfg.rrf_k, rank);
            slot.1 = Some(bm25);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LexicalConfig {
        LexicalConfig {
            enabled: true,
            rrf_k: 60.0,
            w_dense: 1.0,
            w_lex: 1.0,
            ..LexicalConfig::default()
        }
    }

    #[test]
    fn a_chunk_top_in_both_arms_scores_highest() {
        // 3 chunks. Chunk 0 is rank-0 in both arms; chunk 1 only dense; chunk 2
        // only lexical. Chunk 0 must dominate (two arms beat one).
        let fused = fuse(3, &[0, 1, 2], &[(0, 9.0), (2, 5.0)], &cfg());
        assert!(fused[0].0 > fused[1].0, "two-arm hit beats dense-only");
        assert!(fused[0].0 > fused[2].0, "two-arm hit beats lexical-only");
        // Provenance: chunks 0 and 2 matched the lexical arm; chunk 1 did not.
        assert_eq!(fused[0].1, Some(9.0));
        assert_eq!(fused[1].1, None, "dense-only chunk has no lexical score");
        assert_eq!(fused[2].1, Some(5.0));
    }

    #[test]
    fn lexical_only_hit_is_surfaced_with_a_real_score() {
        // Chunk 2 is invisible to dense (not in dense_order at all) but rank-0
        // lexical — it still gets a positive fused score from the lexical arm.
        let fused = fuse(3, &[0, 1], &[(2, 7.0)], &cfg());
        assert!(fused[2].0 > 0.0, "lexical-only chunk surfaces");
        assert_eq!(fused[2].1, Some(7.0));
    }

    #[test]
    fn rank_is_what_matters_not_raw_bm25_magnitude() {
        // RRF fuses on RANK, so a huge BM25 magnitude doesn't dominate — only the
        // arm's rank position does. Chunk 0 at lexical rank 0 (bm25 1000) and
        // chunk 1 at lexical rank 1 (bm25 1) differ only by the rank term.
        let fused = fuse(2, &[1, 0], &[(0, 1000.0), (1, 1.0)], &cfg());
        let d0 = fused[0].0; // dense rank 1 + lex rank 0
        let d1 = fused[1].0; // dense rank 0 + lex rank 1
        // Both have one rank-0 and one rank-1 contribution ⇒ equal fused score.
        assert!((d0 - d1).abs() < 1e-12, "fusion is rank-based, magnitude-blind");
    }

    #[test]
    fn weights_shift_the_balance() {
        // Clean comparison: chunk 0 is dense-top-only (absent from lexical),
        // chunk 1 is lexical-top-only (absent from dense). Under symmetric
        // weights they tie; dense-favoured weighting lifts the dense-top above.
        let sym = cfg();
        let fused = fuse(2, &[0], &[(1, 5.0)], &sym);
        assert!((fused[0].0 - fused[1].0).abs() < 1e-12, "symmetric ⇒ tie");

        let dense_fav = LexicalConfig {
            enabled: true,
            rrf_k: 60.0,
            w_dense: 2.0,
            w_lex: 1.0,
            ..LexicalConfig::default()
        };
        let fused = fuse(2, &[0], &[(1, 5.0)], &dense_fav);
        assert!(
            fused[0].0 > fused[1].0,
            "dense-favoured weight ranks the dense-top chunk above the lexical-top"
        );
    }

    #[test]
    fn empty_arms_yield_zero_scores() {
        let fused = fuse(2, &[], &[], &cfg());
        assert_eq!(fused, vec![(0.0, None), (0.0, None)]);
    }
}
