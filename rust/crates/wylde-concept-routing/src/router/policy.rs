//! The selection policy — *how many* of the top-scoring concepts to activate,
//! given the score distribution.
//!
//! This is a faithful reuse of the **`dynamic_k` cutoff shape** from the
//! battle-tested raw-vector RAG core
//! (`wylde-workspaces/src/rag/indexer/search.rs::dynamic_k`): absolute floor +
//! relative floor + budget cap, taken as a contiguous descending prefix. The
//! RAG cutoff is already calibrated; routing inherits its discipline rather
//! than inventing a new one (plan §6.1). The only difference is the floors are
//! config-driven (`abs_threshold` / `relative_floor`) because centroid cosines
//! are means and run flatter than chunk cosines (plan §8 risk 1).
//!
//! Pure + deterministic: takes a *descending-sorted* score slice and the
//! config knobs, returns how many lead entries activate.

/// The cutoff a concept must clear to activate, and how many do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cutoff {
    /// `max(abs_threshold, relative_floor · top)` once the top clears
    /// `abs_threshold`; otherwise `abs_threshold` itself (nothing activates).
    pub threshold: f32,
    /// Count of leading concepts that activate (`0..=max_concepts`).
    pub activated: usize,
}

/// Decide the cutoff + activation count over `scores` (the per-concept cosines,
/// **sorted descending**), capped at `max_concepts`.
///
/// Returns, mirroring `rank_with::dynamic_k`:
/// * `activated == 0` — the best concept is below `abs_threshold`: nothing on
///   topic, so route nothing (the clean fallback signal → raw RAG).
/// * `activated == 1` — one concept dominates (the rest fall below
///   `relative_floor · top`): don't dilute it.
/// * up to `max_concepts` — several concepts cluster near the top.
///
/// Because `scores` is descending, the activated set is the contiguous prefix
/// clearing `max(abs_threshold, relative_floor · top)`, capped at
/// `max_concepts`. `threshold` is always reported (for the calibration log)
/// even when nothing activates.
pub fn select(scores: &[f32], abs_threshold: f32, relative_floor: f32, max_concepts: usize) -> Cutoff {
    let Some(&top) = scores.first() else {
        return Cutoff {
            threshold: abs_threshold,
            activated: 0,
        };
    };

    // Best concept is noise → activate nothing. Report the absolute floor as
    // the cutoff so the log shows "needed `abs_threshold`, top was `top`".
    if top < abs_threshold {
        return Cutoff {
            threshold: abs_threshold,
            activated: 0,
        };
    }

    let threshold = abs_threshold.max(relative_floor * top);
    let activated = scores
        .iter()
        .take(max_concepts)
        .take_while(|&&s| s >= threshold)
        .count()
        // `top` cleared `threshold` by construction, so at least the dominant
        // concept is always kept once we're here.
        .max(1);

    Cutoff {
        threshold,
        activated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror the RAG dynamic-k test discipline: assert behaviour stays
    // meaningful regardless of the exact float thresholds.
    const ABS: f32 = 0.50;
    const REL: f32 = 0.6;

    #[test]
    fn empty_activates_nothing() {
        let c = select(&[], ABS, REL, 3);
        assert_eq!(c.activated, 0);
        assert_eq!(c.threshold, ABS);
    }

    #[test]
    fn best_below_absolute_floor_activates_nothing() {
        let c = select(&[ABS - 0.05, ABS - 0.10], ABS, REL, 3);
        assert_eq!(c.activated, 0, "off-topic ⇒ route nothing ⇒ fall back to RAG");
        assert_eq!(c.threshold, ABS);
    }

    #[test]
    fn one_dominant_concept_activates_alone() {
        // top = 0.9 ⇒ relative threshold = 0.54; the tail (0.40) is below both.
        let c = select(&[0.9, 0.40, 0.20], ABS, REL, 3);
        assert_eq!(c.activated, 1, "don't dilute a clear winner with weak tail");
        assert!((c.threshold - (REL * 0.9)).max(ABS) >= ABS);
    }

    #[test]
    fn a_cluster_near_the_top_all_activate() {
        // top = 0.80 ⇒ relative threshold = 0.48 → capped to abs 0.50.
        // 0.80, 0.72, 0.66 all clear 0.50.
        let c = select(&[0.80, 0.72, 0.66, 0.30], ABS, REL, 5);
        assert_eq!(c.activated, 3);
    }

    #[test]
    fn max_concepts_caps_the_prefix() {
        let c = select(&[0.9, 0.88, 0.86, 0.84, 0.82], ABS, REL, 2);
        assert_eq!(c.activated, 2, "budget cap honoured even on a wide cluster");
    }

    #[test]
    fn relative_floor_excludes_a_midpack_tail() {
        // top = 0.95 ⇒ relative threshold = 0.57. 0.60 clears it, 0.55 (above
        // the absolute 0.50 but below 0.57) does not — relative floor bites.
        let c = select(&[0.95, 0.60, 0.55], ABS, REL, 5);
        assert_eq!(c.activated, 2);
        assert!((c.threshold - 0.57).abs() < 1e-6);
    }

    #[test]
    fn top_exactly_at_absolute_floor_activates() {
        let c = select(&[ABS, ABS - 0.2], ABS, REL, 3);
        assert_eq!(c.activated, 1);
    }
}
