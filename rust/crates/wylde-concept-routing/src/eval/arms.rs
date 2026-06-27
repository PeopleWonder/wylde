//! The three retrieval **arms** (concept-routing plan §6.4) and the **relation
//! ablation** — the heart of the eval.
//!
//! * **A — Baseline**: plain-vector RAG. Top-`k` files by best-chunk cosine.
//! * **B — Augment**: routed concept files PREPENDED to the baseline, padded to
//!   `k` (the shipped default: the concept slot rides *alongside* RAG).
//! * **C — Replace**: routed concept files only; falls back to baseline when
//!   routing activates nothing (never ship an empty retrieval — plan §3).
//!
//! The **ablation** runs the routing arms under [`RelationMode::SeedOnly`]
//! (empty relation graph ⇒ pure R1 seed) vs [`RelationMode::RelationsOn`] (the
//! authored typed edges drive spreading activation). This is the real claim
//! (addendum §6.2): *does the relation graph beat plain seed routing?*
//!
//! Every arm is **pure** and runs the *real* routing code — [`route`] (seed +
//! spread + `policy::select`) and [`apply_curation`] (budget eviction) — so the
//! eval measures the shipped decision layer, not a re-implementation. Only the
//! corpus embeddings differ between a live run and a fixture run.

use std::collections::HashMap;

use crate::config::RoutingConfig;
use crate::curation::apply_curation;
use crate::curation::candidate::PER_CONCEPT_INJECT_TOKENS;
use crate::relations::RelationGraph;
use crate::router::{match_vocabulary, route, CandidateSet};

use super::corpus::EvalCorpus;

/// Which retrieval strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arm {
    Baseline,
    Augment,
    Replace,
}

impl Arm {
    pub fn as_str(self) -> &'static str {
        match self {
            Arm::Baseline => "baseline",
            Arm::Augment => "augment",
            Arm::Replace => "replace",
        }
    }
    /// Baseline ignores relations; the routing arms honour the ablation.
    pub fn uses_routing(self) -> bool {
        !matches!(self, Arm::Baseline)
    }
}

/// Relation ablation switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationMode {
    /// Empty relation graph ⇒ pure-seed routing (R1).
    SeedOnly,
    /// The authored typed edges drive spreading activation.
    RelationsOn,
}

impl RelationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationMode::SeedOnly => "seed-only",
            RelationMode::RelationsOn => "relations-on",
        }
    }
}

/// How many `match_vocabulary` terms a query may match (mirrors the live cap).
const VOCAB_MATCH_LIMIT: usize = 8;

/// The result of running one arm on one query.
#[derive(Clone, Debug)]
pub struct ArmRun {
    /// Retrieved files, best-first, de-duplicated, capped at `k`.
    pub ranked_files: Vec<String>,
    /// Estimated tokens injected (the bloat axis).
    pub injected_tokens: usize,
    /// Activated concept ids (empty for baseline / when routing fired nothing).
    pub activated_concepts: Vec<String>,
    /// The routing decision (None for baseline), for the conflation/dependency
    /// metrics + diagnostics.
    pub candidate: Option<CandidateSet>,
    /// True when a routing arm returned no activation and fell back to baseline.
    pub fell_back: bool,
}

/// Average chunk token cost over the corpus (the per-file representative-chunk
/// cost the baseline pays for each retrieved file). `0` for an empty corpus.
fn avg_chunk_tokens(corpus: &EvalCorpus) -> usize {
    if corpus.chunks.is_empty() {
        return 0;
    }
    let total: usize = corpus.chunks.iter().map(|c| c.tokens).sum();
    (total / corpus.chunks.len()).max(1)
}

/// Run `arm` under `relmode` over `corpus` for an already-embedded query.
///
/// `query_text` is still passed so vocabulary matching (and, live, the
/// anchor/active-file markers) work off the text; `query_vec` is the embedding
/// the live RAG already paid for (no extra embed — plan §6.1).
pub fn run_arm(
    corpus: &EvalCorpus,
    query_vec: &[f32],
    query_text: &str,
    cfg: &RoutingConfig,
    arm: Arm,
    relmode: RelationMode,
    k: usize,
) -> ArmRun {
    let file_cos = corpus.file_best_cosine(query_vec);
    let baseline_ranked: Vec<String> = corpus
        .baseline_ranked_files(query_vec)
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    let per_file_tokens = avg_chunk_tokens(corpus);

    if arm == Arm::Baseline {
        let files: Vec<String> = baseline_ranked.into_iter().take(k).collect();
        let injected_tokens = files.len() * per_file_tokens;
        return ArmRun {
            ranked_files: files,
            injected_tokens,
            activated_concepts: Vec::new(),
            candidate: None,
            fell_back: false,
        };
    }

    // ── Routing arms: run the REAL router under the chosen relation mode ──────
    let centroids = corpus.concept_centroids();
    let empty;
    let graph: &RelationGraph = match relmode {
        RelationMode::SeedOnly => {
            empty = RelationGraph::empty();
            &empty
        }
        RelationMode::RelationsOn => &corpus.relations,
    };
    let vocab = match_vocabulary(query_text, &corpus.vocab_terms, VOCAB_MATCH_LIMIT);
    // The eval harness does not exercise the H6 containment channel (it has no
    // hierarchy overlay); pass an empty adjacency so routing is identity w.r.t.
    // containment and the eval numbers stay comparable to pre-H6.
    let cand = route(query_text, query_vec, &centroids, vocab, &[], graph, cfg);
    let activated_ids: Vec<String> = cand.activated().map(|c| c.id.clone()).collect();

    // Auto-curate = inject every activated concept (the menu's "auto next time"
    // path), trimmed by the real budget eviction.
    let plan = apply_curation(&activated_ids, &cand, cfg.inject_token_budget);
    let inject_ids = &plan.concepts;

    // The files a routed concept contributes, ranked by their best-chunk cosine
    // to the query (concepts pick the candidate set; the index ranks within it).
    let concept_by_id: HashMap<&str, &super::corpus::EvalConcept> =
        corpus.concepts.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut routed_files: Vec<String> = Vec::new();
    for id in inject_ids {
        if let Some(c) = concept_by_id.get(id.as_str()) {
            for f in &c.member_files {
                if !routed_files.contains(f) {
                    routed_files.push(f.clone());
                }
            }
        }
    }
    routed_files.sort_by(|a, b| {
        let ca = file_cos.get(a).copied().unwrap_or(f32::MIN);
        let cb = file_cos.get(b).copied().unwrap_or(f32::MIN);
        cb.partial_cmp(&ca)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });

    let inject_tokens = inject_ids.len() * PER_CONCEPT_INJECT_TOKENS;

    let (ranked_files, injected_tokens, fell_back) = match arm {
        Arm::Augment => {
            // Routed files first, then baseline fill, deduped, capped at k.
            let mut out = routed_files.clone();
            for f in &baseline_ranked {
                if out.len() >= k {
                    break;
                }
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
            out.truncate(k);
            let baseline_tokens = baseline_ranked.iter().take(k).count() * per_file_tokens;
            (out, baseline_tokens + inject_tokens, false)
        }
        Arm::Replace => {
            if inject_ids.is_empty() {
                // Routing fired nothing ⇒ never ship empty: fall back to RAG.
                let files: Vec<String> = baseline_ranked.iter().take(k).cloned().collect();
                let tokens = files.len() * per_file_tokens;
                (files, tokens, true)
            } else {
                let files: Vec<String> = routed_files.iter().take(k).cloned().collect();
                (files, inject_tokens, false)
            }
        }
        Arm::Baseline => unreachable!("handled above"),
    };

    ArmRun {
        ranked_files,
        injected_tokens,
        activated_concepts: activated_ids,
        candidate: Some(cand),
        fell_back,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::corpus::{EvalChunk, EvalConcept};
    use crate::relations::{NodeRef, Relation, RelationKind};

    /// A tiny 2-D corpus: two concepts, one on-axis with the query.
    fn corpus() -> EvalCorpus {
        EvalCorpus {
            chunks: vec![
                EvalChunk {
                    path: "src/auth/login.rs".into(),
                    vector: vec![1.0, 0.0],
                    tokens: 100,
                },
                EvalChunk {
                    path: "src/graph/draw.rs".into(),
                    vector: vec![0.0, 1.0],
                    tokens: 100,
                },
            ],
            concepts: vec![
                EvalConcept {
                    id: "auth".into(),
                    label: "Auth".into(),
                    centroid: vec![1.0, 0.0],
                    member_files: vec!["src/auth/login.rs".into()],
                    described_by: vec![],
                },
                EvalConcept {
                    id: "graph".into(),
                    label: "Graph".into(),
                    centroid: vec![0.0, 1.0],
                    member_files: vec!["src/graph/draw.rs".into()],
                    described_by: vec![],
                },
            ],
            relations: RelationGraph::empty(),
            vocab_terms: vec![],
        }
    }

    fn cfg() -> RoutingConfig {
        RoutingConfig {
            enabled: true,
            abs_threshold: 0.5,
            relative_floor: 0.6,
            max_concepts: 3,
            ..RoutingConfig::default()
        }
    }

    #[test]
    fn baseline_ranks_by_cosine() {
        let run = run_arm(
            &corpus(),
            &[1.0, 0.0],
            "auth",
            &cfg(),
            Arm::Baseline,
            RelationMode::SeedOnly,
            2,
        );
        assert_eq!(run.ranked_files[0], "src/auth/login.rs");
        assert!(run.activated_concepts.is_empty());
        assert_eq!(run.injected_tokens, 2 * 100);
    }

    #[test]
    fn replace_returns_only_routed_files() {
        let run = run_arm(
            &corpus(),
            &[1.0, 0.0],
            "auth",
            &cfg(),
            Arm::Replace,
            RelationMode::SeedOnly,
            5,
        );
        assert_eq!(run.activated_concepts, vec!["auth"]);
        assert_eq!(run.ranked_files, vec!["src/auth/login.rs"]);
        assert!(!run.fell_back);
        // Token cost is the concept injection, not the baseline.
        assert_eq!(run.injected_tokens, PER_CONCEPT_INJECT_TOKENS);
    }

    #[test]
    fn replace_falls_back_when_nothing_activates() {
        // Query orthogonal to every centroid ⇒ below the floor ⇒ no activation.
        let c = EvalCorpus {
            concepts: vec![EvalConcept {
                id: "auth".into(),
                label: "Auth".into(),
                centroid: vec![0.0, 1.0],
                member_files: vec!["src/auth/login.rs".into()],
                described_by: vec![],
            }],
            ..corpus()
        };
        let run = run_arm(
            &c,
            &[1.0, 0.0],
            "x",
            &cfg(),
            Arm::Replace,
            RelationMode::SeedOnly,
            5,
        );
        assert!(run.fell_back, "no activation ⇒ baseline fallback");
        assert!(!run.ranked_files.is_empty());
    }

    #[test]
    fn augment_prepends_routed_then_fills_baseline() {
        let run = run_arm(
            &corpus(),
            &[1.0, 0.0],
            "auth",
            &cfg(),
            Arm::Augment,
            RelationMode::SeedOnly,
            2,
        );
        // Routed (auth) first; baseline fills the rest.
        assert_eq!(run.ranked_files[0], "src/auth/login.rs");
        assert!(run.ranked_files.contains(&"src/graph/draw.rs".to_string()));
        // Augment cost = baseline slot + the concept injection.
        assert_eq!(run.injected_tokens, 2 * 100 + PER_CONCEPT_INJECT_TOKENS);
    }

    #[test]
    fn relations_on_uses_the_authored_graph() {
        // Exclusion edge auth ⊘ graph: when both seed near-equal, RelationsOn
        // suppresses graph. Here the query is on the auth axis so auth already
        // wins; assert the ablation switch threads the graph through (relations
        // present vs empty changes the candidate's reshaped flag).
        let mut c = corpus();
        c.relations = RelationGraph {
            relations: vec![Relation::normalized(
                NodeRef::concept("auth"),
                NodeRef::concept("graph"),
                RelationKind::Negative,
                None,
            )],
        };
        let q = vec![1.0, 0.3]; // both concepts get some seed
        let seed = run_arm(&c, &q, "q", &cfg(), Arm::Replace, RelationMode::SeedOnly, 5);
        let rel = run_arm(
            &c,
            &q,
            "q",
            &cfg(),
            Arm::Replace,
            RelationMode::RelationsOn,
            5,
        );
        // The seed-only run never reshapes; the relations-on run does (the
        // exclusion damps graph's activation), proving the ablation switch
        // actually threads the authored graph into the real router.
        assert!(!seed.candidate.as_ref().unwrap().reshaped_by_relations());
        assert!(rel.candidate.as_ref().unwrap().reshaped_by_relations());
    }
}
