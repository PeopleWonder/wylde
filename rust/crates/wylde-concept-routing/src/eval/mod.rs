//! Eval harness — does concept-routed retrieval beat raw-vector RAG?
//! (concept-routing plan §6.4; relation-model addendum §6 — the thesis claim).
//!
//! ## Shape (R4)
//!
//! A **pure, deterministic** harness that runs three arms — **A baseline**
//! (plain-vector RAG), **B augment** (concept-routed, additive), **C replace**
//! (routed only) — plus the **relation ablation** (seed-only vs relations-on)
//! over a gold set, scoring recall@k / nDCG@k / precision@k and the token-cost
//! axis. Every arm runs the *real* routing code ([`crate::route`] /
//! [`crate::apply_curation`]); only the corpus embeddings differ between a live
//! run and a fixture run, so the same numbers are reproducible offline.
//!
//! * [`gold`] — the `(query, relevant-files)` gold set ([`GoldSet`], embedded
//!   DRAFT fixture).
//! * [`corpus`] — the in-memory [`EvalCorpus`] (chunks + concepts + relations)
//!   the arms run over, + path-matching + grading.
//! * [`metrics`] — recall@k / precision@k / nDCG@k / token cost.
//! * [`arms`] — the three [`Arm`]s + the [`RelationMode`] ablation.
//! * [`harness`] — [`run_eval`] (the matrix) + [`sweep_abs_threshold`]
//!   (calibration).
//!
//! ## Live vs simulated
//!
//! The harness is corpus-agnostic. The `#[ignore]`d `tests/live_eval.rs` driver
//! builds an [`EvalCorpus`] from the **live** persisted index (`chunks.jsonl`,
//! real `nomic-embed-text` vectors) + the decrypted concept store, embeds the
//! gold queries against the **running** Ollama, and runs this harness — so the
//! baseline-RAG and routing-retrieval numbers are *measured*. Relations are
//! hand-authored for the gold conflation/dependency pairs (no relations are
//! persisted live yet), so the ablation is *measured on live seeds with
//! authored edges*. The in-crate tests run the harness over controlled,
//! clean-concept corpora — the *mechanism* proof — which is *simulated*.

pub mod arms;
pub mod corpus;
pub mod gold;
pub mod harness;
pub mod metrics;

pub use arms::{run_arm, Arm, ArmRun, RelationMode};
pub use corpus::{grade, normalize_path, path_matches, EvalChunk, EvalConcept, EvalCorpus};
pub use gold::{CaseKind, GoldCase, GoldSet};
pub use harness::{run_eval, sweep_abs_threshold, ArmAgg, CaseResult, EvalReport, SweepPoint};
pub use metrics::{f1, mean, ndcg_at_k, precision_at_k, recall_at_k};

/// Render an [`EvalReport`] as a Markdown table block — used by the live driver
/// to write `outputs/concept-routing-r4-eval-results.md`. Pure string building.
pub fn render_report_markdown(report: &EvalReport, title: &str) -> String {
    use std::fmt::Write;
    let (easy, conf, dep) = report.gold_counts;
    let mut s = String::new();
    let _ = writeln!(s, "## {title}\n");
    let _ = writeln!(
        s,
        "Gold: {} cases graded (easy {easy} / conflation {conf} / dependency {dep}); k={}.\n",
        report.graded_cases, report.k
    );
    let _ = writeln!(
        s,
        "| arm | relations | recall@k | nDCG@k | precision@k | mean tokens | mean activated | fallback | conflation-suppress | dep-recover |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|");
    for a in &report.aggs {
        let conf_s = a
            .conflation_suppression_rate
            .map(|v| format!("{:.0}%", v * 100.0))
            .unwrap_or_else(|| "—".into());
        let dep_s = a
            .dependency_recovery_rate
            .map(|v| format!("{:.0}%", v * 100.0))
            .unwrap_or_else(|| "—".into());
        let _ = writeln!(
            s,
            "| {} | {} | {:.3} | {:.3} | {:.3} | {:.0} | {:.2} | {:.0}% | {} | {} |",
            a.arm.as_str(),
            a.relmode.as_str(),
            a.mean_recall,
            a.mean_ndcg,
            a.mean_precision,
            a.mean_tokens,
            a.mean_activated,
            a.fallback_rate * 100.0,
            conf_s,
            dep_s,
        );
    }
    s
}

/// Render a threshold sweep as a Markdown table.
pub fn render_sweep_markdown(points: &[SweepPoint], title: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "## {title}\n");
    let _ = writeln!(
        s,
        "| abs_threshold | recall@k | nDCG@k | mean tokens | mean activated | routed-nothing |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|");
    for p in points {
        let _ = writeln!(
            s,
            "| {:.2} | {:.3} | {:.3} | {:.0} | {:.2} | {:.0}% |",
            p.abs_threshold,
            p.mean_recall,
            p.mean_ndcg,
            p.mean_tokens,
            p.mean_activated,
            p.routed_nothing_rate * 100.0,
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;
    use std::collections::HashMap;

    #[test]
    fn render_report_is_nonempty_markdown() {
        let corpus = EvalCorpus {
            chunks: vec![EvalChunk {
                path: "a.rs".into(),
                vector: vec![1.0, 0.0],
                tokens: 50,
            }],
            concepts: vec![EvalConcept {
                id: "c".into(),
                label: "C".into(),
                centroid: vec![1.0, 0.0],
                member_files: vec!["a.rs".into()],
                described_by: vec![],
            }],
            ..EvalCorpus::default()
        };
        let gold = GoldSet {
            version: "t".into(),
            cases: vec![GoldCase {
                id: "q".into(),
                kind: CaseKind::Easy,
                query: "x".into(),
                relevant_files: vec!["a.rs".into()],
                avoid_files: vec![],
                dependency_files: vec![],
                concepts: vec![],
            }],
        };
        let mut qv = HashMap::new();
        qv.insert("q".to_string(), vec![1.0, 0.0]);
        let cfg = RoutingConfig {
            enabled: true,
            ..RoutingConfig::default()
        };
        let report = run_eval(
            &corpus,
            &gold,
            &qv,
            &cfg,
            3,
            &[Arm::Baseline, Arm::Replace],
            &[RelationMode::SeedOnly],
        );
        let md = render_report_markdown(&report, "Test");
        assert!(md.contains("| arm |"));
        assert!(md.contains("baseline"));
        assert!(md.contains("replace"));
    }
}
