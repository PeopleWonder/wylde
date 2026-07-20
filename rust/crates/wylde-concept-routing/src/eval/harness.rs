//! The arm runner + aggregation + calibration sweep (concept-routing plan §6.4;
//! relation-model addendum §6).
//!
//! [`run_eval`] runs every requested `(arm, relation-mode)` over the whole gold
//! set with pre-embedded query vectors, grading each arm's ranked files against
//! the case's `relevant_files` and folding the per-case metrics into one
//! [`ArmAgg`] per combination. [`sweep_abs_threshold`] is the calibration tool:
//! it replays the gold set at a range of absolute thresholds so the flat-cosine
//! finding (R1) is visible and a tuned default can be picked.
//!
//! Pure + deterministic — given the same corpus + query vectors it produces the
//! same report every run (the live driver supplies the vectors from Ollama; a
//! test supplies them from a synthetic corpus).

use std::collections::HashMap;

use crate::config::RoutingConfig;

use super::arms::{run_arm, Arm, RelationMode};
use super::corpus::{grade, path_matches, EvalCorpus};
use super::gold::{CaseKind, GoldSet};
use super::metrics::{mean, ndcg_at_k, precision_at_k, recall_at_k};

/// One case under one arm/relation-mode.
#[derive(Clone, Debug)]
pub struct CaseResult {
    pub id: String,
    pub kind: CaseKind,
    pub arm: Arm,
    pub relmode: RelationMode,
    pub recall: f64,
    pub precision: f64,
    pub ndcg: f64,
    pub injected_tokens: usize,
    pub activated_count: usize,
    pub fell_back: bool,
    /// Conflation cases only: did the top-`k` keep every `avoid_file` OUT?
    pub conflation_suppressed: Option<bool>,
    /// Dependency cases only: did the top-`k` pull in any `dependency_file`?
    pub dependency_recovered: Option<bool>,
}

/// Aggregated metrics for one `(arm, relation-mode)` over the gold set.
#[derive(Clone, Debug)]
pub struct ArmAgg {
    pub arm: Arm,
    pub relmode: RelationMode,
    pub n: usize,
    pub mean_recall: f64,
    pub mean_precision: f64,
    pub mean_ndcg: f64,
    pub mean_tokens: f64,
    pub mean_activated: f64,
    pub fallback_rate: f64,
    /// Over conflation cases: fraction that kept all avoid-files out.
    pub conflation_suppression_rate: Option<f64>,
    /// Over dependency cases: fraction that pulled in a dependency file.
    pub dependency_recovery_rate: Option<f64>,
}

/// The whole eval result.
#[derive(Clone, Debug)]
pub struct EvalReport {
    pub k: usize,
    /// (easy, conflation, dependency) gold counts.
    pub gold_counts: (usize, usize, usize),
    pub graded_cases: usize,
    pub cases: Vec<CaseResult>,
    pub aggs: Vec<ArmAgg>,
}

impl EvalReport {
    /// The aggregate for one combination, if it was run.
    pub fn agg(&self, arm: Arm, relmode: RelationMode) -> Option<&ArmAgg> {
        self.aggs
            .iter()
            .find(|a| a.arm == arm && a.relmode == relmode)
    }
}

/// Run the full eval. `query_vecs` maps a gold case id → its embedded query;
/// cases without a vector (an embed that failed live) are skipped. `arms` /
/// `relmodes` choose the matrix; baseline is always run once (it ignores
/// relations).
#[allow(clippy::too_many_arguments)] // an eval entry point: corpus + gold + vecs + cfg + matrix
pub fn run_eval(
    corpus: &EvalCorpus,
    gold: &GoldSet,
    query_vecs: &HashMap<String, Vec<f32>>,
    cfg: &RoutingConfig,
    k: usize,
    arms: &[Arm],
    relmodes: &[RelationMode],
) -> EvalReport {
    let mut cases: Vec<CaseResult> = Vec::new();
    let mut graded = 0usize;

    for case in &gold.cases {
        let Some(qv) = query_vecs.get(&case.id) else {
            continue; // no embedding for this case — skip (counted out)
        };
        graded += 1;
        for &arm in arms {
            // Baseline is relation-agnostic: run it once under SeedOnly.
            let modes: &[RelationMode] = if arm.uses_routing() {
                relmodes
            } else {
                &[RelationMode::SeedOnly]
            };
            for &relmode in modes {
                let run = run_arm(corpus, qv, &case.query, cfg, arm, relmode, k);
                let graded_vec = grade(&run.ranked_files, &case.relevant_files);
                let total_rel = case.relevant_files.len();
                let recall = recall_at_k(&graded_vec, total_rel, k);
                let precision = precision_at_k(&graded_vec, k);
                let ndcg = ndcg_at_k(&graded_vec, total_rel, k);

                let conflation_suppressed = if case.kind == CaseKind::Conflation {
                    Some(!any_path_in(&run.ranked_files, &case.avoid_files, k))
                } else {
                    None
                };
                let dependency_recovered = if case.kind == CaseKind::Dependency {
                    Some(any_path_in(&run.ranked_files, &case.dependency_files, k))
                } else {
                    None
                };

                cases.push(CaseResult {
                    id: case.id.clone(),
                    kind: case.kind,
                    arm,
                    relmode,
                    recall,
                    precision,
                    ndcg,
                    injected_tokens: run.injected_tokens,
                    activated_count: run.activated_concepts.len(),
                    fell_back: run.fell_back,
                    conflation_suppressed,
                    dependency_recovered,
                });
            }
        }
    }

    let aggs = aggregate(&cases);
    EvalReport {
        k,
        gold_counts: gold.counts(),
        graded_cases: graded,
        cases,
        aggs,
    }
}

/// True when any of `needles` appears in the top-`k` of `ranked` (suffix match).
fn any_path_in(ranked: &[String], needles: &[String], k: usize) -> bool {
    ranked
        .iter()
        .take(k)
        .any(|r| needles.iter().any(|n| path_matches(r, n)))
}

/// Fold per-case results into one [`ArmAgg`] per `(arm, relmode)`.
fn aggregate(cases: &[CaseResult]) -> Vec<ArmAgg> {
    // Stable combination order: arm then relmode.
    let mut combos: Vec<(Arm, RelationMode)> = Vec::new();
    for c in cases {
        if !combos.contains(&(c.arm, c.relmode)) {
            combos.push((c.arm, c.relmode));
        }
    }
    combos.sort();

    combos
        .into_iter()
        .map(|(arm, relmode)| {
            let group: Vec<&CaseResult> = cases
                .iter()
                .filter(|c| c.arm == arm && c.relmode == relmode)
                .collect();
            let recalls: Vec<f64> = group.iter().map(|c| c.recall).collect();
            let precisions: Vec<f64> = group.iter().map(|c| c.precision).collect();
            let ndcgs: Vec<f64> = group.iter().map(|c| c.ndcg).collect();
            let tokens: Vec<f64> = group.iter().map(|c| c.injected_tokens as f64).collect();
            let activated: Vec<f64> = group.iter().map(|c| c.activated_count as f64).collect();
            let fallback =
                group.iter().filter(|c| c.fell_back).count() as f64 / (group.len().max(1) as f64);

            let conf: Vec<f64> = group
                .iter()
                .filter_map(|c| c.conflation_suppressed)
                .map(|b| if b { 1.0 } else { 0.0 })
                .collect();
            let dep: Vec<f64> = group
                .iter()
                .filter_map(|c| c.dependency_recovered)
                .map(|b| if b { 1.0 } else { 0.0 })
                .collect();

            ArmAgg {
                arm,
                relmode,
                n: group.len(),
                mean_recall: mean(&recalls),
                mean_precision: mean(&precisions),
                mean_ndcg: mean(&ndcgs),
                mean_tokens: mean(&tokens),
                mean_activated: mean(&activated),
                fallback_rate: fallback,
                conflation_suppression_rate: (!conf.is_empty()).then(|| mean(&conf)),
                dependency_recovery_rate: (!dep.is_empty()).then(|| mean(&dep)),
            }
        })
        .collect()
}

/// One point on the threshold-calibration curve.
#[derive(Clone, Debug)]
pub struct SweepPoint {
    pub abs_threshold: f32,
    pub mean_recall: f64,
    pub mean_ndcg: f64,
    pub mean_tokens: f64,
    pub mean_activated: f64,
    /// Fraction of queries where routing activated NOTHING (fell back to RAG).
    pub routed_nothing_rate: f64,
}

/// Sweep `abs_threshold` for one arm/relation-mode over the gold set — the
/// calibration tool. Shows how activation count + nDCG move as the floor rises:
/// on the live flat-cosine distribution a low floor activates the cap on every
/// query (R1), and the curve reveals where the floor starts discriminating.
#[allow(clippy::too_many_arguments)] // a calibration entry point: corpus + gold + vecs + cfg + axes
pub fn sweep_abs_threshold(
    corpus: &EvalCorpus,
    gold: &GoldSet,
    query_vecs: &HashMap<String, Vec<f32>>,
    base_cfg: &RoutingConfig,
    arm: Arm,
    relmode: RelationMode,
    k: usize,
    thresholds: &[f32],
) -> Vec<SweepPoint> {
    thresholds
        .iter()
        .map(|&t| {
            let cfg = RoutingConfig {
                abs_threshold: t,
                ..*base_cfg
            };
            let mut recalls = Vec::new();
            let mut ndcgs = Vec::new();
            let mut tokens = Vec::new();
            let mut activated = Vec::new();
            let mut nothing = 0usize;
            let mut n = 0usize;
            for case in &gold.cases {
                let Some(qv) = query_vecs.get(&case.id) else {
                    continue;
                };
                n += 1;
                let run = run_arm(corpus, qv, &case.query, &cfg, arm, relmode, k);
                let g = grade(&run.ranked_files, &case.relevant_files);
                recalls.push(recall_at_k(&g, case.relevant_files.len(), k));
                ndcgs.push(ndcg_at_k(&g, case.relevant_files.len(), k));
                tokens.push(run.injected_tokens as f64);
                activated.push(run.activated_concepts.len() as f64);
                if run.activated_concepts.is_empty() {
                    nothing += 1;
                }
            }
            SweepPoint {
                abs_threshold: t,
                mean_recall: mean(&recalls),
                mean_ndcg: mean(&ndcgs),
                mean_tokens: mean(&tokens),
                mean_activated: mean(&activated),
                routed_nothing_rate: nothing as f64 / (n.max(1) as f64),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::corpus::{EvalChunk, EvalConcept};
    use crate::relations::{NodeRef, Relation, RelationGraph, RelationKind};

    /// A controlled corpus reproducing the intended clean-concept world: 4
    /// concepts on orthogonal axes, one chunk per member file, plus the gold +
    /// query vectors. This is the MECHANISM proof corpus (simulated, clean).
    fn clean_corpus() -> (EvalCorpus, GoldSet, HashMap<String, Vec<f32>>) {
        // 4-D space, one axis per concept.
        let concepts = [
            (
                "auth",
                "Authentication",
                "src/auth/login.rs",
                [1.0, 0.0, 0.0, 0.0],
            ),
            (
                "rag",
                "Retrieval",
                "src/rag/search.rs",
                [0.0, 1.0, 0.0, 0.0],
            ),
            (
                "thumbnailer",
                "Thumbnailer",
                "src/media/thumbnailer.rs",
                [0.0, 0.0, 1.0, 0.0],
            ),
            (
                "photos",
                "Photos",
                "src/media/photos.rs",
                [0.0, 0.0, 0.0, 1.0],
            ),
        ];
        let chunks = concepts
            .iter()
            .map(|(_, _, f, v)| EvalChunk {
                path: (*f).into(),
                vector: v.to_vec(),
                tokens: 100,
            })
            .collect();
        let ev_concepts = concepts
            .iter()
            .map(|(id, label, f, v)| EvalConcept {
                id: (*id).into(),
                label: (*label).into(),
                centroid: v.to_vec(),
                member_files: vec![(*f).into()],
                described_by: vec![],
            })
            .collect();
        let corpus = EvalCorpus {
            chunks,
            concepts: ev_concepts,
            relations: RelationGraph::empty(),
            vocab_terms: vec![],
        };
        let gold = GoldSet {
            version: "test".into(),
            cases: vec![super::super::gold::GoldCase {
                id: "ph".into(),
                kind: CaseKind::Easy,
                query: "photos".into(),
                relevant_files: vec!["src/media/photos.rs".into()],
                avoid_files: vec![],
                dependency_files: vec![],
                concepts: vec![],
            }],
        };
        let mut qv = HashMap::new();
        // Query points mostly at photos but flatly close to thumbnailer (the flat-
        // cosine condition).
        qv.insert("ph".into(), vec![0.0, 0.0, 0.55, 0.83]);
        (corpus, gold, qv)
    }

    #[test]
    fn run_eval_produces_aggs_for_every_combo() {
        let (corpus, gold, qv) = clean_corpus();
        let cfg = RoutingConfig {
            enabled: true,
            abs_threshold: 0.5,
            ..RoutingConfig::default()
        };
        let report = run_eval(
            &corpus,
            &gold,
            &qv,
            &cfg,
            3,
            &[Arm::Baseline, Arm::Augment, Arm::Replace],
            &[RelationMode::SeedOnly, RelationMode::RelationsOn],
        );
        // Baseline: 1 combo. Augment + Replace: 2 relation modes each ⇒ 5 aggs.
        assert_eq!(report.aggs.len(), 5);
        assert!(report.agg(Arm::Baseline, RelationMode::SeedOnly).is_some());
        assert!(report
            .agg(Arm::Replace, RelationMode::RelationsOn)
            .is_some());
        assert_eq!(report.graded_cases, 1);
    }

    #[test]
    fn dependency_recovered_only_with_relations() {
        // Query fires Photos; Thumbnailer sits flat below the floor. A
        // Photos→Thumbnailer dependency edge should pull Thumbnailer's file in under
        // RelationsOn but not SeedOnly.
        let (mut corpus, _g, mut qv) = clean_corpus();
        corpus.relations = RelationGraph {
            relations: vec![Relation::normalized(
                NodeRef::concept("photos"),
                NodeRef::concept("thumbnailer"),
                RelationKind::Dependency,
                None,
            )],
        };
        let gold = GoldSet {
            version: "t".into(),
            cases: vec![super::super::gold::GoldCase {
                id: "dep".into(),
                kind: CaseKind::Dependency,
                query: "photos".into(),
                relevant_files: vec!["src/media/photos.rs".into()],
                avoid_files: vec![],
                dependency_files: vec!["src/media/thumbnailer.rs".into()],
                concepts: vec![],
            }],
        };
        qv.clear();
        // Strong on photos, weak (flat, below the relative floor) on thumbnailer.
        qv.insert("dep".into(), vec![0.0, 0.0, 0.20, 0.98]);
        // CALIBRATION FINDING baked into the test: a 1-hop dependency reaches at
        // most `dep_decay × top`, so it can only clear `relative_floor × top`
        // when `dep_decay > relative_floor`. With the addendum defaults
        // (dep_decay 0.5 < relative_floor 0.6) a dependency NEVER activates —
        // R4 raises dep_decay above the relative floor. Here: dep_decay 0.7,
        // relative_floor 0.5, abs lowered to 0.3 (discrimination delegated to
        // the relation, not the scalar floor).
        let cfg = RoutingConfig {
            enabled: true,
            abs_threshold: 0.3,
            relative_floor: 0.5,
            max_concepts: 3,
            relation_params: crate::config::RelationParams {
                dep_decay: 0.7,
                ..crate::config::RelationParams::default()
            },
            ..RoutingConfig::default()
        };
        let report = run_eval(
            &corpus,
            &gold,
            &qv,
            &cfg,
            5,
            &[Arm::Replace],
            &[RelationMode::SeedOnly, RelationMode::RelationsOn],
        );
        let seed = report.agg(Arm::Replace, RelationMode::SeedOnly).unwrap();
        let rel = report.agg(Arm::Replace, RelationMode::RelationsOn).unwrap();
        assert_eq!(
            seed.dependency_recovery_rate,
            Some(0.0),
            "pure seed misses the flat dependency"
        );
        assert_eq!(
            rel.dependency_recovery_rate,
            Some(1.0),
            "the dependency edge pulls Thumbnailer in"
        );
    }

    #[test]
    fn conflation_suppressed_by_exclusion() {
        // Query fires Photos; Wylde (an off-topic neighbour) sits flat just
        // below. With an authored Photos ⊘ Wylde edge the Wylde file must
        // stay out of the Replace results.
        let concepts = [
            ("photos", "Photos", "src/media/photos.rs", [1.0, 0.0]),
            ("wylde", "Wylde", "src/wylde/core.rs", [0.80, 0.60]),
        ];
        let chunks = concepts
            .iter()
            .map(|(_, _, f, v)| EvalChunk {
                path: (*f).into(),
                vector: v.to_vec(),
                tokens: 100,
            })
            .collect();
        let ev_concepts = concepts
            .iter()
            .map(|(id, label, f, v)| EvalConcept {
                id: (*id).into(),
                label: (*label).into(),
                centroid: v.to_vec(),
                member_files: vec![(*f).into()],
                described_by: vec![],
            })
            .collect();
        let corpus = EvalCorpus {
            chunks,
            concepts: ev_concepts,
            relations: RelationGraph {
                relations: vec![Relation::normalized(
                    NodeRef::concept("photos"),
                    NodeRef::concept("wylde"),
                    RelationKind::Negative,
                    None,
                )],
            },
            vocab_terms: vec![],
        };
        let gold = GoldSet {
            version: "t".into(),
            cases: vec![super::super::gold::GoldCase {
                id: "conf".into(),
                kind: CaseKind::Conflation,
                query: "photos".into(),
                relevant_files: vec!["src/media/photos.rs".into()],
                avoid_files: vec!["src/wylde/core.rs".into()],
                dependency_files: vec![],
                concepts: vec![],
            }],
        };
        let mut qv = HashMap::new();
        qv.insert("conf".into(), vec![1.0, 0.0]);
        // abs lowered to 0.3 so the on-topic concept survives the symmetric
        // inhibition damp (which also nicks the firing node); the exclusion then
        // opens the gap that pushes the conflated Wylde below the cutoff.
        let cfg = RoutingConfig {
            enabled: true,
            abs_threshold: 0.3,
            relative_floor: 0.6,
            max_concepts: 3,
            ..RoutingConfig::default()
        };
        let report = run_eval(
            &corpus,
            &gold,
            &qv,
            &cfg,
            5,
            &[Arm::Replace],
            &[RelationMode::SeedOnly, RelationMode::RelationsOn],
        );
        let seed = report.agg(Arm::Replace, RelationMode::SeedOnly).unwrap();
        let rel = report.agg(Arm::Replace, RelationMode::RelationsOn).unwrap();
        // Seed-only: with cosine 0.92 vs 1.0, relative floor 0.6 → both clear
        // 0.6, so Wylde activates and leaks. Relations-on suppresses it.
        assert_eq!(
            rel.conflation_suppression_rate,
            Some(1.0),
            "exclusion keeps the conflated file out"
        );
        assert!(
            seed.conflation_suppression_rate.unwrap() <= rel.conflation_suppression_rate.unwrap(),
            "relations never suppress worse than seed-only"
        );
    }

    #[test]
    fn sweep_shows_activation_falling_as_threshold_rises() {
        let (corpus, gold, qv) = clean_corpus();
        let cfg = RoutingConfig {
            enabled: true,
            max_concepts: 3,
            ..RoutingConfig::default()
        };
        let pts = sweep_abs_threshold(
            &corpus,
            &gold,
            &qv,
            &cfg,
            Arm::Replace,
            RelationMode::SeedOnly,
            5,
            &[0.1, 0.5, 0.9, 0.99],
        );
        assert_eq!(pts.len(), 4);
        // Monotone: a higher floor never activates MORE concepts.
        for w in pts.windows(2) {
            assert!(
                w[1].mean_activated <= w[0].mean_activated + 1e-9,
                "activation should not rise with the floor"
            );
        }
        // At a near-1.0 floor nothing clears it.
        assert!(pts.last().unwrap().routed_nothing_rate > 0.0);
    }
}
