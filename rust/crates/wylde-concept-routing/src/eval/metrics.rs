//! Retrieval metrics (concept-routing plan §6.4) — recall@k, precision@k,
//! nDCG@k, and the token-cost axis, plus the small aggregation helpers the
//! harness folds per-case results into.
//!
//! Pure + deterministic. Every metric takes a **graded relevance vector** — a
//! `&[bool]` where entry `i` is "the file at rank `i` is relevant" — plus the
//! total number of relevant items in the gold (needed for recall + the ideal
//! DCG). Grading a ranked list of paths against a gold list is
//! [`crate::eval::corpus::grade`]; keeping the metrics over `&[bool]` keeps
//! them independent of how relevance was decided (suffix match, concept
//! membership, etc.).

/// Fraction of the gold's relevant items that appear in the top-`k`.
/// `total_relevant == 0` ⇒ `0.0` (an undefined recall is reported as zero, not
/// NaN, so it averages cleanly).
pub fn recall_at_k(graded: &[bool], total_relevant: usize, k: usize) -> f64 {
    if total_relevant == 0 {
        return 0.0;
    }
    let hits = graded.iter().take(k).filter(|&&b| b).count();
    hits as f64 / total_relevant as f64
}

/// Fraction of the top-`k` that is relevant. `k == 0` ⇒ `0.0`. The denominator
/// is the **smaller** of `k` and the number of results actually returned (a
/// run that returns 3 files for `k=10` is scored over 3, not penalised for the
/// 7 empty slots — the token-cost axis already captures "returned less").
pub fn precision_at_k(graded: &[bool], k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let denom = graded.len().min(k);
    if denom == 0 {
        return 0.0;
    }
    let hits = graded.iter().take(k).filter(|&&b| b).count();
    hits as f64 / denom as f64
}

/// Normalised discounted cumulative gain at `k`, binary gains. `DCG = Σ rel_i /
/// log2(i+2)`; the ideal DCG places `min(total_relevant, k)` relevant items
/// first. Returns `0.0` when there is nothing relevant to find.
pub fn ndcg_at_k(graded: &[bool], total_relevant: usize, k: usize) -> f64 {
    if total_relevant == 0 || k == 0 {
        return 0.0;
    }
    let dcg: f64 = graded
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, &b)| b)
        .map(|(i, _)| 1.0 / ((i as f64) + 2.0).log2())
        .sum();
    let ideal_hits = total_relevant.min(k);
    let idcg: f64 = (0..ideal_hits)
        .map(|i| 1.0 / ((i as f64) + 2.0).log2())
        .sum();
    if idcg <= 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

/// Harmonic mean of precision + recall (`0.0` when both are zero).
pub fn f1(precision: f64, recall: f64) -> f64 {
    if precision + recall <= 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

/// Mean of a slice (`0.0` for an empty slice).
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn recall_counts_hits_over_total_relevant() {
        // 2 of 4 gold items found in the top-3.
        let g = [true, false, true, false];
        assert!(approx(recall_at_k(&g, 4, 3), 0.5));
        // top-1 finds only 1 of 4.
        assert!(approx(recall_at_k(&g, 4, 1), 0.25));
    }

    #[test]
    fn recall_undefined_is_zero_not_nan() {
        assert_eq!(recall_at_k(&[true], 0, 5), 0.0);
    }

    #[test]
    fn precision_over_returned_count_when_short() {
        // Only 2 results returned, both relevant ⇒ precision@10 = 1.0 (not 0.2).
        let g = [true, true];
        assert!(approx(precision_at_k(&g, 10), 1.0));
        // 1 of 4 in the top-4.
        let g = [true, false, false, false];
        assert!(approx(precision_at_k(&g, 4), 0.25));
    }

    #[test]
    fn ndcg_perfect_ranking_is_one() {
        // All relevant items first ⇒ DCG == IDCG.
        let g = [true, true, false, false];
        assert!(approx(ndcg_at_k(&g, 2, 4), 1.0));
    }

    #[test]
    fn ndcg_rewards_earlier_hits() {
        let early = [true, false, false, false];
        let late = [false, false, false, true];
        assert!(ndcg_at_k(&early, 1, 4) > ndcg_at_k(&late, 1, 4));
        // A hit at rank 4 with 1 relevant: DCG = 1/log2(5), IDCG = 1/log2(2)=1.
        assert!(approx(ndcg_at_k(&late, 1, 4), 1.0 / 5f64.log2()));
    }

    #[test]
    fn f1_balances_precision_recall() {
        assert!(approx(f1(0.5, 0.5), 0.5));
        assert_eq!(f1(0.0, 0.0), 0.0);
        assert!(approx(f1(1.0, 0.5), 2.0 / 3.0));
    }

    #[test]
    fn mean_handles_empty() {
        assert_eq!(mean(&[]), 0.0);
        assert!(approx(mean(&[1.0, 2.0, 3.0]), 2.0));
    }
}
