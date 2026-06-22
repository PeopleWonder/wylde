//! Apply the user's curation choices to a routed [`CandidateSet`] — diff the
//! checked set against the routed set, enforce the injection token budget, and
//! produce the final list of concept ids to inject (concept-routing plan §4
//! "apply", §6.3 budget; relation-model addendum §4).
//!
//! Pure. The two-phase turn calls this server-side (R2): `chat.run_turn` carries
//! the user-curated concept ids; [`apply_curation`] resolves them against the
//! routed candidates, drops any unknown id, and — when the curated set's
//! estimated token cost exceeds the budget — **evicts the lowest-activation
//! concept first** (plan §6.3) until it fits, recording what was shed so the
//! caller can warn.
//!
//! **Empty curated set ⇒ empty plan ⇒ nothing injected** (Aaron's lock: a
//! curated-empty menu must inject nothing). An unknown id is simply ignored —
//! a stale menu can never inject a concept the router didn't surface.

use serde::{Deserialize, Serialize};

use super::candidate::PER_CONCEPT_INJECT_TOKENS;
use crate::router::CandidateSet;

/// The resolved injection plan: which concept ids to inject, in activation
/// order, plus what the budget eviction shed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InjectionPlan {
    /// Concept ids to inject, best-activation first. Empty ⇒ inject nothing.
    pub concepts: Vec<String>,
    /// Concept ids dropped to fit the token budget (lowest-activation first).
    pub evicted: Vec<String>,
    /// True when the curated set was trimmed to fit the budget.
    pub over_budget: bool,
}

impl InjectionPlan {
    /// Nothing to inject — the Augment fallback (the existing RAG slot still
    /// runs; the concept slot is empty).
    pub fn is_empty(&self) -> bool {
        self.concepts.is_empty()
    }
}

/// Resolve `curated_ids` against `set` under `inject_token_budget`.
///
/// * Keeps only ids the router actually surfaced (drops unknown/stale ids).
/// * Orders the kept concepts by settled activation, descending (so the
///   strongest concept is injected first and survives budget eviction longest).
/// * When the estimated token cost exceeds the budget, evicts the
///   **lowest-activation** concept first until it fits, recording each in
///   `evicted` and setting `over_budget`.
///
/// The per-concept token estimate mirrors [`super::candidate`]: a flat blurb
/// cost plus an even share of the remaining budget across the kept set. Because
/// the share shrinks as the set grows, a large curated set is trimmed from the
/// bottom until the survivors each clear a sane floor.
pub fn apply_curation(
    curated_ids: &[String],
    set: &CandidateSet,
    inject_token_budget: usize,
) -> InjectionPlan {
    // Resolve to (id, activation), keeping only router-surfaced concepts, in
    // the caller's order first; we re-sort by activation next.
    let mut kept: Vec<(String, f32)> = curated_ids
        .iter()
        .filter_map(|id| {
            set.concepts
                .iter()
                .find(|c| &c.id == id)
                .map(|c| (c.id.clone(), c.score))
        })
        .collect();
    // De-dup (a stale menu could repeat an id) preserving the first occurrence.
    let mut seen = std::collections::HashSet::new();
    kept.retain(|(id, _)| seen.insert(id.clone()));

    // Activation order, descending; stable id tiebreak (deterministic).
    kept.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    if kept.is_empty() {
        return InjectionPlan::default();
    }

    // Evict the lowest-activation concept (the tail) until the estimated cost
    // (flat PER_CONCEPT_INJECT_TOKENS each) fits the budget. Always keep ≥1 so a
    // tiny budget still injects the single strongest concept.
    let mut evicted: Vec<String> = Vec::new();
    while kept.len() > 1 && kept.len() * PER_CONCEPT_INJECT_TOKENS > inject_token_budget {
        // The tail is the lowest activation (kept is sorted desc).
        let (id, _) = kept.pop().expect("len > 1");
        evicted.push(id);
    }

    InjectionPlan {
        concepts: kept.into_iter().map(|(id, _)| id).collect(),
        over_budget: !evicted.is_empty(),
        evicted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::spread::Provenance;
    use crate::router::{RoutedConcept, VocabMatch};

    fn concept(id: &str, score: f32) -> RoutedConcept {
        RoutedConcept {
            id: id.into(),
            label: id.to_uppercase(),
            score,
            seed_score: score,
            provenance: Provenance::Seed,
            activated: true,
        }
    }

    fn set(concepts: Vec<RoutedConcept>) -> CandidateSet {
        CandidateSet {
            query_echo: "q".into(),
            concepts,
            vocabulary: Vec::<VocabMatch>::new(),
            abs_threshold: 0.5,
            chosen_cutoff: 0.5,
            activated_count: 0,
            max_concepts: 3,
        }
    }

    #[test]
    fn empty_curation_injects_nothing() {
        let cs = set(vec![concept("a", 0.7)]);
        let plan = apply_curation(&[], &cs, 1500);
        assert!(plan.is_empty());
        assert!(!plan.over_budget);
    }

    #[test]
    fn keeps_only_router_surfaced_ids_in_activation_order() {
        let cs = set(vec![concept("a", 0.5), concept("b", 0.8)]);
        // Curated in a deliberately wrong order, with a stale id.
        let plan = apply_curation(&["a".into(), "ghost".into(), "b".into()], &cs, 5000);
        // "ghost" dropped; ordered by activation desc (b 0.8 before a 0.5).
        assert_eq!(plan.concepts, vec!["b".to_owned(), "a".to_owned()]);
        assert!(!plan.over_budget);
    }

    #[test]
    fn dedupes_repeated_ids() {
        let cs = set(vec![concept("a", 0.7)]);
        let plan = apply_curation(&["a".into(), "a".into()], &cs, 5000);
        assert_eq!(plan.concepts, vec!["a".to_owned()]);
    }

    #[test]
    fn over_budget_evicts_lowest_activation_first() {
        let cs = set(vec![
            concept("hi", 0.9),
            concept("mid", 0.6),
            concept("lo", 0.3),
        ]);
        // A budget that fits exactly one concept (PER_CONCEPT_INJECT_TOKENS ==
        // 230, so 2× overflows 300): the tail is trimmed to the single
        // strongest.
        let budget = 300;
        let plan = apply_curation(&["hi".into(), "mid".into(), "lo".into()], &cs, budget);
        // The lowest-activation concepts are shed first; at least the top stays.
        assert_eq!(plan.concepts, vec!["hi".to_owned()]);
        assert!(plan.over_budget);
        assert!(plan.evicted.contains(&"lo".to_owned()));
        assert!(plan.evicted.contains(&"mid".to_owned()));
    }

    #[test]
    fn within_budget_keeps_all() {
        let cs = set(vec![concept("a", 0.7), concept("b", 0.6)]);
        let plan = apply_curation(&["a".into(), "b".into()], &cs, 100_000);
        assert_eq!(plan.concepts.len(), 2);
        assert!(!plan.over_budget);
        assert!(plan.evicted.is_empty());
    }
}
