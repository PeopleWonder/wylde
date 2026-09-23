//! Eval harness for the lexical/BM25 + RRF fusion (lexical-bm25 plan L7 / §5).
//!
//! **Does fused retrieval beat dense-only on the queries the embedder
//! structurally fails — and not hurt the ones it nails?** Three arms over the
//! same chunk corpus:
//!
//! * **Dense** — cosine ranking (today's signal).
//! * **Lexical** — BM25 over the tantivy index ([`super::indexer::lexical`]).
//! * **Fused** — RRF of the two ([`super::indexer::fuse`], the proposal).
//!
//! graded against a gold set with a **lexical class** (exact identifiers, error
//! codes, rare tokens the embedder blurs) and a **semantic class** (the "easy"
//! queries dense already nails — the no-regression guardrail). The
//! recall@k / nDCG@k / precision@k math + suffix grading are **reused verbatim**
//! from `wylde_concept_routing::eval` (corpus-agnostic, pure); only the arms +
//! the gold class are new.
//!
//! ## Pure vs index-backed
//!
//! The dense + fused *rankings* are pure (cosine + RRF over in-memory vectors);
//! the lexical arm reads a real tantivy index on disk (BM25 is local — no
//! embedder), so the whole harness runs in CI over a synthetic corpus (the
//! mechanism proof, [`tests`]) and identically over the **live** `chunks.jsonl`
//! via the `#[ignore]`d `tests/lexical_eval.rs` driver (the measured numbers).
//!
//! The rankings here deliberately **omit the production dynamic-k cutoff + MMR**
//! and rank the full candidate set top-k, so recall@k measures *ranking* quality
//! in isolation (a low-cosine relevant file the dense arm buries at rank 40 vs
//! the fused arm lifting it into the top-k). The cutoff itself — the off-topic
//! guard + the approved low-cosine bypass — is calibrated separately by
//! [`calibrate_floor`].

use std::collections::HashMap;

use wylde_concept_routing::eval::{grade, mean, ndcg_at_k, precision_at_k, recall_at_k};

use super::indexer::{fuse, lexical, store};
use super::{cosine, LexicalConfig};

/// One corpus chunk in the eval's input shape (the join of what each arm needs:
/// a vector for cosine, a body for BM25, an id+path for the join + grading).
#[derive(Clone, Debug)]
pub struct EvalChunk {
    pub id: String,
    pub path: String,
    pub content: String,
    pub vector: Vec<f32>,
}

/// Which structural gap a gold case exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexClass {
    /// An exact identifier / error code / rare token the embedder can't recover
    /// — where the lexical arm is expected to win.
    Lexical,
    /// A topical query dense already nails — the no-regression guardrail.
    Semantic,
    /// A query that should ground nothing (off-topic to both arms) — used only
    /// by [`calibrate_floor`] to confirm the cutoff still injects nothing.
    OffTopic,
}

/// One gold case: a query + the files that should ground it (suffix-matched).
#[derive(Clone, Debug)]
pub struct LexGoldCase {
    pub id: String,
    pub query: String,
    pub relevant_files: Vec<String>,
    pub class: LexClass,
}

/// The three retrieval arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    Dense,
    Lexical,
    Fused,
}

impl Arm {
    pub fn as_str(&self) -> &'static str {
        match self {
            Arm::Dense => "dense",
            Arm::Lexical => "lexical",
            Arm::Fused => "fused",
        }
    }
}

/// How deep to fetch the lexical arm before fusing (mirrors `search.rs`).
const LEXICAL_FETCH: usize = 50;

/// Persist `chunks` + build the lexical index for `ws` so the lexical/fused arms
/// have something to query. Call once before [`run_eval`] / the sweeps.
pub fn build_corpus_index(ws: &str, chunks: &[EvalChunk]) {
    let indexed: Vec<store::IndexedChunk> = chunks
        .iter()
        .map(|c| store::IndexedChunk {
            id: c.id.clone(),
            path: c.path.clone(),
            chunk_idx: 0,
            content: c.content.clone(),
            mtime: 1.0,
            start_line: 1,
            end_line: 1,
            vector: c.vector.clone(),
        })
        .collect();
    let _ = store::save_chunks(ws, &indexed);
    let _ = lexical::build_from_chunks(ws, &indexed);
}

/// Collapse `(file, score)` rows to a best-per-file ranking, descending, top-k.
fn best_per_file_topk(rows: Vec<(String, f64)>, k: usize) -> Vec<String> {
    let mut best: HashMap<String, f64> = HashMap::new();
    for (f, s) in rows {
        best.entry(f)
            .and_modify(|b| {
                if s > *b {
                    *b = s;
                }
            })
            .or_insert(s);
    }
    let mut files: Vec<(String, f64)> = best.into_iter().collect();
    files.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    files.into_iter().take(k).map(|(f, _)| f).collect()
}

/// Dense arm: every file at its best cosine, descending, top-k.
fn dense_ranked_files(chunks: &[EvalChunk], qv: &[f32], k: usize) -> Vec<String> {
    let rows = chunks
        .iter()
        .map(|c| (c.path.clone(), cosine(qv, &c.vector)))
        .collect();
    best_per_file_topk(rows, k)
}

/// Lexical arm: best BM25 per file from [`lexical::search_boosted`], top-k.
fn lexical_ranked_files(ws: &str, chunks: &[EvalChunk], text: &str, k: usize) -> Vec<String> {
    let id_to_path: HashMap<&str, &str> = chunks
        .iter()
        .map(|c| (c.id.as_str(), c.path.as_str()))
        .collect();
    let rows = lexical::search_boosted(ws, text, &[], LEXICAL_FETCH)
        .into_iter()
        .filter_map(|(id, s)| id_to_path.get(id.as_str()).map(|p| ((*p).to_owned(), s)))
        .collect();
    best_per_file_topk(rows, k)
}

/// The per-chunk fused scores for a query: `(fused, cosine, lexical_opt)`,
/// parallel to `chunks`. Shared by [`fused_ranked_files`] and [`calibrate_floor`].
fn fused_scores(
    ws: &str,
    chunks: &[EvalChunk],
    qv: &[f32],
    text: &str,
    cfg: &LexicalConfig,
) -> Vec<(f64, f64, Option<f64>)> {
    let n = chunks.len();
    let cosines: Vec<f64> = chunks.iter().map(|c| cosine(qv, &c.vector)).collect();
    let mut dense_order: Vec<usize> = (0..n).collect();
    dense_order.sort_by(|&a, &b| {
        cosines[b]
            .partial_cmp(&cosines[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let id_to_idx: HashMap<&str, usize> = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    let lex_hits: Vec<(usize, f64)> = lexical::search_boosted(ws, text, &[], LEXICAL_FETCH)
        .into_iter()
        .filter_map(|(id, s)| id_to_idx.get(id.as_str()).map(|&i| (i, s)))
        .collect();
    let fused = fuse::fuse(n, &dense_order, &lex_hits, cfg);
    (0..n)
        .map(|i| (fused[i].0, cosines[i], fused[i].1))
        .collect()
}

/// Fused arm: best RRF score per file, descending, top-k.
fn fused_ranked_files(
    ws: &str,
    chunks: &[EvalChunk],
    qv: &[f32],
    text: &str,
    k: usize,
    cfg: &LexicalConfig,
) -> Vec<String> {
    let scores = fused_scores(ws, chunks, qv, text, cfg);
    let rows = chunks
        .iter()
        .zip(scores)
        .map(|(c, (fused, _, _))| (c.path.clone(), fused))
        .collect();
    best_per_file_topk(rows, k)
}

/// The ranked file list one arm produces for one case.
pub fn run_arm(
    arm: Arm,
    ws: &str,
    chunks: &[EvalChunk],
    qv: &[f32],
    text: &str,
    k: usize,
    cfg: &LexicalConfig,
) -> Vec<String> {
    match arm {
        Arm::Dense => dense_ranked_files(chunks, qv, k),
        Arm::Lexical => lexical_ranked_files(ws, chunks, text, k),
        Arm::Fused => fused_ranked_files(ws, chunks, qv, text, k, cfg),
    }
}

/// Aggregated metrics for one `(arm, class)` over the gold set.
#[derive(Clone, Debug)]
pub struct ArmClassAgg {
    pub arm: Arm,
    pub class: LexClass,
    pub n: usize,
    pub recall: f64,
    pub ndcg: f64,
    pub precision: f64,
}

/// The whole eval result.
#[derive(Clone, Debug)]
pub struct EvalReport {
    pub k: usize,
    pub aggs: Vec<ArmClassAgg>,
}

impl EvalReport {
    pub fn agg(&self, arm: Arm, class: LexClass) -> Option<&ArmClassAgg> {
        self.aggs.iter().find(|a| a.arm == arm && a.class == class)
    }
}

/// Run all three arms over every gradeable gold case (excluding `OffTopic`,
/// which has no relevant files), aggregating recall@k / nDCG@k / precision@k per
/// `(arm, class)`. `query_vecs` maps a case id → its embedded query (a case
/// without a vector is skipped — an embed that failed live).
pub fn run_eval(
    ws: &str,
    chunks: &[EvalChunk],
    gold: &[LexGoldCase],
    query_vecs: &HashMap<String, Vec<f32>>,
    k: usize,
    cfg: &LexicalConfig,
) -> EvalReport {
    let arms = [Arm::Dense, Arm::Lexical, Arm::Fused];
    let classes = [LexClass::Lexical, LexClass::Semantic];
    let mut aggs = Vec::new();
    for &arm in &arms {
        for &class in &classes {
            let mut recalls = Vec::new();
            let mut ndcgs = Vec::new();
            let mut precisions = Vec::new();
            for case in gold.iter().filter(|c| c.class == class) {
                let Some(qv) = query_vecs.get(&case.id) else {
                    continue;
                };
                let ranked = run_arm(arm, ws, chunks, qv, &case.query, k, cfg);
                let graded = grade(&ranked, &case.relevant_files);
                let total = case.relevant_files.len();
                recalls.push(recall_at_k(&graded, total, k));
                ndcgs.push(ndcg_at_k(&graded, total, k));
                precisions.push(precision_at_k(&graded, k));
            }
            aggs.push(ArmClassAgg {
                arm,
                class,
                n: recalls.len(),
                recall: mean(&recalls),
                ndcg: mean(&ndcgs),
                precision: mean(&precisions),
            });
        }
    }
    EvalReport { k, aggs }
}

/// Mirror of `search.rs::dynamic_k_fused` (the production cutoff) over a sorted
/// fused candidate list `(fused, cosine, lexical_opt)` — replicated here so the
/// calibration measures the exact gate without the `IndexedChunk` coupling.
fn fused_cutoff_count(scored: &[(f64, f64, Option<f64>)], k: usize, cfg: &LexicalConfig) -> usize {
    if k == 0 || scored.is_empty() {
        return 0;
    }
    let (top_fused, top_cos, top_lex) = scored[0];
    let on_topic = top_cos >= 0.55 || top_lex.map(|bm| bm >= cfg.min_bm25).unwrap_or(false);
    if !on_topic || top_fused <= 0.0 {
        return 0;
    }
    let threshold = cfg.fused_relative_floor * top_fused;
    scored
        .iter()
        .take(k)
        .take_while(|c| c.0 >= threshold)
        .count()
        .max(1)
}

/// One point on the cutoff-floor calibration curve.
#[derive(Clone, Debug)]
pub struct FloorPoint {
    pub fused_relative_floor: f64,
    /// Fraction of `Lexical` cases whose relevant file is INJECTED (cutoff keeps
    /// it). The bypass must keep this high — that is the recall win.
    pub lexical_inject_rate: f64,
    /// Fraction of `OffTopic` cases that inject NOTHING (cutoff = 0). Must stay
    /// 1.0 — the off-topic guard.
    pub offtopic_silent_rate: f64,
    /// Mean kept count over `Semantic` cases (sanity: dense-nailed queries still
    /// fill the budget).
    pub semantic_mean_kept: f64,
}

/// Sweep the relative floor and report, per point, whether the lexical bypass
/// still fires and the off-topic guard still holds — the data that LANDS the
/// `fused_relative_floor` cutoff (§1.4). The chosen floor is the largest value
/// that keeps `lexical_inject_rate` at 1.0 while `offtopic_silent_rate` stays
/// 1.0 (tightest cutoff that loses no recall and admits no noise).
pub fn calibrate_floor(
    ws: &str,
    chunks: &[EvalChunk],
    gold: &[LexGoldCase],
    query_vecs: &HashMap<String, Vec<f32>>,
    k: usize,
    base_cfg: &LexicalConfig,
    floors: &[f64],
) -> Vec<FloorPoint> {
    floors
        .iter()
        .map(|&floor| {
            let cfg = LexicalConfig {
                fused_relative_floor: floor,
                ..*base_cfg
            };
            let mut lex_inject = Vec::new();
            let mut off_silent = Vec::new();
            let mut sem_kept = Vec::new();
            for case in gold {
                let Some(qv) = query_vecs.get(&case.id) else {
                    continue;
                };
                let mut scored = fused_scores(ws, chunks, qv, &case.query, &cfg);
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                let keep = fused_cutoff_count(&scored, k, &cfg);
                match case.class {
                    LexClass::Lexical => {
                        // Injected iff a relevant file is within the kept prefix.
                        let kept_files =
                            fused_ranked_files(ws, chunks, qv, &case.query, keep, &cfg);
                        let hit = grade(&kept_files, &case.relevant_files).iter().any(|&b| b);
                        lex_inject.push(if keep > 0 && hit { 1.0 } else { 0.0 });
                    }
                    LexClass::OffTopic => off_silent.push(if keep == 0 { 1.0 } else { 0.0 }),
                    LexClass::Semantic => sem_kept.push(keep as f64),
                }
            }
            FloorPoint {
                fused_relative_floor: floor,
                lexical_inject_rate: mean(&lex_inject),
                offtopic_silent_rate: mean(&off_silent),
                semantic_mean_kept: mean(&sem_kept),
            }
        })
        .collect()
}

/// One point on the RRF-parameter sweep (recall/nDCG of the fused arm).
#[derive(Clone, Debug)]
pub struct SweepPoint {
    pub rrf_k: f64,
    pub w_dense: f64,
    pub w_lex: f64,
    pub lexical_recall: f64,
    pub lexical_ndcg: f64,
    pub semantic_recall: f64,
    pub semantic_ndcg: f64,
}

/// Sweep `(rrf_k, w_dense, w_lex)` for the fused arm, reporting recall@k/nDCG@k
/// on the lexical class (the win axis) and the semantic class (the guardrail).
pub fn sweep_rrf(
    ws: &str,
    chunks: &[EvalChunk],
    gold: &[LexGoldCase],
    query_vecs: &HashMap<String, Vec<f32>>,
    k: usize,
    base_cfg: &LexicalConfig,
    points: &[(f64, f64, f64)],
) -> Vec<SweepPoint> {
    points
        .iter()
        .map(|&(rrf_k, w_dense, w_lex)| {
            let cfg = LexicalConfig {
                rrf_k,
                w_dense,
                w_lex,
                ..*base_cfg
            };
            let report = run_eval(ws, chunks, gold, query_vecs, k, &cfg);
            let lex = report.agg(Arm::Fused, LexClass::Lexical);
            let sem = report.agg(Arm::Fused, LexClass::Semantic);
            SweepPoint {
                rrf_k,
                w_dense,
                w_lex,
                lexical_recall: lex.map(|a| a.recall).unwrap_or(0.0),
                lexical_ndcg: lex.map(|a| a.ndcg).unwrap_or(0.0),
                semantic_recall: sem.map(|a| a.recall).unwrap_or(0.0),
                semantic_ndcg: sem.map(|a| a.ndcg).unwrap_or(0.0),
            }
        })
        .collect()
}

/// Render the per-`(arm, class)` report as a Markdown table.
pub fn render_report_markdown(report: &EvalReport, title: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "## {title}\n");
    let _ = writeln!(s, "k = {}.\n", report.k);
    let _ = writeln!(s, "| arm | class | n | recall@k | nDCG@k | precision@k |");
    let _ = writeln!(s, "|---|---|---|---|---|---|");
    for a in &report.aggs {
        let class = match a.class {
            LexClass::Lexical => "lexical",
            LexClass::Semantic => "semantic",
            LexClass::OffTopic => "off-topic",
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {:.3} | {:.3} | {:.3} |",
            a.arm.as_str(),
            class,
            a.n,
            a.recall,
            a.ndcg,
            a.precision
        );
    }
    s
}

/// Render the RRF-parameter sweep as a Markdown table.
pub fn render_sweep_markdown(points: &[SweepPoint], title: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "## {title}\n");
    let _ = writeln!(
        s,
        "| rrf_k | w_dense | w_lex | lexical recall | lexical nDCG | semantic recall | semantic nDCG |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|");
    for p in points {
        let _ = writeln!(
            s,
            "| {:.0} | {:.1} | {:.1} | {:.3} | {:.3} | {:.3} | {:.3} |",
            p.rrf_k,
            p.w_dense,
            p.w_lex,
            p.lexical_recall,
            p.lexical_ndcg,
            p.semantic_recall,
            p.semantic_ndcg
        );
    }
    s
}

/// Render the floor calibration as a Markdown table.
pub fn render_floor_markdown(points: &[FloorPoint], title: &str) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "## {title}\n");
    let _ = writeln!(
        s,
        "| fused_relative_floor | lexical inject | off-topic silent | semantic mean kept |"
    );
    let _ = writeln!(s, "|---|---|---|---|");
    for p in points {
        let _ = writeln!(
            s,
            "| {:.2} | {:.0}% | {:.0}% | {:.2} |",
            p.fused_relative_floor,
            p.lexical_inject_rate * 100.0,
            p.offtopic_silent_rate * 100.0,
            p.semantic_mean_kept
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    /// Unit vector in 5-D. dim0/1 are the two "semantic topic" axes, dim2/3 hold
    /// the (off-topic-to-the-embedder) lexical-class files, and **dim4 is a pure
    /// off-topic axis no file occupies** — a query pointing there has ~0 cosine
    /// to every chunk, the only way to construct a genuinely off-topic query in a
    /// space the corpus otherwise fills.
    fn vec4(a: f32, b: f32, c: f32, d: f32) -> Vec<f32> {
        vec5(a, b, c, d, 0.0)
    }
    fn vec5(a: f32, b: f32, c: f32, d: f32, e: f32) -> Vec<f32> {
        let v = vec![a, b, c, d, e];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / norm.max(1e-9)).collect()
    }

    /// A synthetic corpus + gold reproducing the two structural cases:
    /// - LEXICAL: the relevant file has a LOW cosine to the query but contains a
    ///   rare exact token only it holds — dense buries it, BM25 (→ fused) recovers.
    /// - SEMANTIC: the relevant file has a HIGH cosine and shares no special
    ///   token — dense nails it; fusion must not hurt it (the guardrail).
    fn synthetic() -> (Vec<EvalChunk>, Vec<LexGoldCase>, HashMap<String, Vec<f32>>) {
        // Axis legend: dim0 = "semantic-A" topic, dim1 = "semantic-B" topic,
        // dim2/3 = noise. Query vectors point along an axis; lexical-class
        // relevant files sit OFF that axis (low cosine).
        let chunks = vec![
            // semantic-A defining file: high cosine to the semA query.
            EvalChunk {
                id: "sa".into(),
                path: "src/auth/login.rs".into(),
                content: "authentication session login token flow".into(),
                vector: vec4(1.0, 0.0, 0.0, 0.0),
            },
            // semantic-B defining file.
            EvalChunk {
                id: "sb".into(),
                path: "src/net/sync.rs".into(),
                content: "synchronise replicate remote folder push".into(),
                vector: vec4(0.0, 1.0, 0.0, 0.0),
            },
            // lexical-class file 1: a rare constant, LOW cosine to every query.
            EvalChunk {
                id: "lx1".into(),
                path: "src/rag/search.rs".into(),
                content: "const ANCHOR_BOOST_CAP f64 0.30 ranking".into(),
                vector: vec4(0.1, 0.1, 1.0, 0.0),
            },
            // lexical-class file 2: an error string, LOW cosine.
            EvalChunk {
                id: "lx2".into(),
                path: "build/guard.rs".into(),
                content: "fail loud os error 32 sharing violation linker".into(),
                vector: vec4(0.1, 0.1, 0.0, 1.0),
            },
            // distractors that share the SEMANTIC topics (so dense fills its top-k
            // with on-axis files, burying the off-axis lexical files).
            EvalChunk {
                id: "d1".into(),
                path: "src/auth/oauth.rs".into(),
                content: "oauth provider authentication grant scope".into(),
                vector: vec4(0.92, 0.1, 0.0, 0.0),
            },
            EvalChunk {
                id: "d2".into(),
                path: "src/auth/session.rs".into(),
                content: "session cookie authentication store".into(),
                vector: vec4(0.88, 0.15, 0.0, 0.0),
            },
            EvalChunk {
                id: "d3".into(),
                path: "src/net/ddns.rs".into(),
                content: "dynamic dns update remote sync".into(),
                vector: vec4(0.1, 0.9, 0.0, 0.0),
            },
        ];
        let gold = vec![
            LexGoldCase {
                id: "semA".into(),
                query: "how does authentication login work".into(),
                relevant_files: vec!["src/auth/login.rs".into()],
                class: LexClass::Semantic,
            },
            LexGoldCase {
                id: "semB".into(),
                query: "how are folders synchronised".into(),
                relevant_files: vec!["src/net/sync.rs".into()],
                class: LexClass::Semantic,
            },
            LexGoldCase {
                id: "lexA".into(),
                query: "ANCHOR_BOOST_CAP".into(),
                relevant_files: vec!["src/rag/search.rs".into()],
                class: LexClass::Lexical,
            },
            LexGoldCase {
                id: "lexB".into(),
                query: "os error 32".into(),
                relevant_files: vec!["build/guard.rs".into()],
                class: LexClass::Lexical,
            },
            // Off-topic to BOTH arms (no shared token, query points at noise axis).
            LexGoldCase {
                id: "off".into(),
                query: "zzqq absent nonsense xyzzy".into(),
                relevant_files: vec![],
                class: LexClass::OffTopic,
            },
        ];
        let mut qv = HashMap::new();
        // Semantic queries point straight along their topic axis (high cosine to
        // the defining file).
        qv.insert("semA".into(), vec4(1.0, 0.0, 0.0, 0.0));
        qv.insert("semB".into(), vec4(0.0, 1.0, 0.0, 0.0));
        // Lexical queries are embedding-BLIND to the rare token: their vector
        // points at a generic topic (auth-ish), so the lexical-class file sits at
        // LOW cosine — only BM25 can recover it.
        qv.insert("lexA".into(), vec4(0.7, 0.3, 0.0, 0.0));
        qv.insert("lexB".into(), vec4(0.7, 0.3, 0.0, 0.0));
        // Off-topic query: points at the empty dim4 (≈0 cosine to every file)
        // and shares no token with any chunk — off-topic to BOTH arms.
        qv.insert("off".into(), vec5(0.0, 0.0, 0.0, 0.0, 1.0));
        (chunks, gold, qv)
    }

    fn eval_cfg() -> LexicalConfig {
        LexicalConfig {
            enabled: true,
            rrf_k: 60.0,
            w_dense: 1.0,
            w_lex: 1.0,
            min_bm25: 0.5,
            fused_relative_floor: 0.6,
            ..LexicalConfig::default()
        }
    }

    #[test]
    fn fused_beats_dense_on_lexical_class_and_holds_on_semantic() {
        let _env = TestEnv::new();
        let ws = "lex-eval";
        let (chunks, gold, qv) = synthetic();
        build_corpus_index(ws, &chunks);
        let k = 3;
        let report = run_eval(ws, &chunks, &gold, &qv, k, &eval_cfg());

        let dense_lex = report.agg(Arm::Dense, LexClass::Lexical).unwrap();
        let lexonly_lex = report.agg(Arm::Lexical, LexClass::Lexical).unwrap();
        let fused_lex = report.agg(Arm::Fused, LexClass::Lexical).unwrap();
        let dense_sem = report.agg(Arm::Dense, LexClass::Semantic).unwrap();
        let fused_sem = report.agg(Arm::Fused, LexClass::Semantic).unwrap();

        // Headline: the embedder buries the rare-token files; BM25 (→ fused)
        // recovers them.
        assert!(
            dense_lex.recall < 0.5,
            "dense buries the lexical class (recall {:.3})",
            dense_lex.recall
        );
        assert!(
            lexonly_lex.recall >= dense_lex.recall,
            "lexical-only ≥ dense on the lexical class"
        );
        assert!(
            fused_lex.recall > dense_lex.recall,
            "fused RECOVERS the lexical class: {:.3} > {:.3}",
            fused_lex.recall,
            dense_lex.recall
        );
        assert!(
            fused_lex.ndcg > dense_lex.ndcg,
            "fused nDCG rises on the lexical class too"
        );
        // Guardrail: fusion does NOT hurt the queries dense already nails.
        assert!(
            fused_sem.recall + 1e-9 >= dense_sem.recall,
            "fused ≈ dense on the semantic class (guardrail): {:.3} vs {:.3}",
            fused_sem.recall,
            dense_sem.recall
        );

        eprintln!(
            "\n{}",
            render_report_markdown(&report, "Mechanism proof (synthetic)")
        );
    }

    #[test]
    fn rrf_sweep_runs_and_prefers_fusion_on_the_lexical_class() {
        let _env = TestEnv::new();
        let ws = "lex-sweep";
        let (chunks, gold, qv) = synthetic();
        build_corpus_index(ws, &chunks);
        let points = sweep_rrf(
            ws,
            &chunks,
            &gold,
            &qv,
            3,
            &eval_cfg(),
            &[
                (60.0, 1.0, 1.0),
                (60.0, 1.0, 2.0),
                (30.0, 1.0, 1.0),
                (10.0, 1.0, 1.0),
            ],
        );
        assert_eq!(points.len(), 4);
        // Every parameterisation recovers SOME lexical recall (fusion engaged).
        assert!(points.iter().all(|p| p.lexical_recall > 0.0));
        // And none collapses the semantic guardrail to zero.
        assert!(points.iter().all(|p| p.semantic_recall > 0.0));
        eprintln!(
            "\n{}",
            render_sweep_markdown(&points, "RRF parameter sweep (synthetic)")
        );
    }

    #[test]
    fn floor_calibration_lands_a_cutoff_that_keeps_recall_and_silences_off_topic() {
        let _env = TestEnv::new();
        let ws = "lex-floor";
        let (chunks, gold, qv) = synthetic();
        build_corpus_index(ws, &chunks);
        let floors = [0.3, 0.5, 0.6, 0.7, 0.8, 0.9];
        let points = calibrate_floor(ws, &chunks, &gold, &qv, 3, &eval_cfg(), &floors);
        // The off-topic guard holds at EVERY floor (the on-topic gate, not the
        // relative floor, silences it).
        assert!(
            points
                .iter()
                .all(|p| (p.offtopic_silent_rate - 1.0).abs() < 1e-9),
            "off-topic stays silent across the floor sweep"
        );
        // There EXISTS a floor that keeps the full lexical recall (the bypass
        // fires) — the chosen production cutoff is the largest such floor.
        let chosen = points
            .iter()
            .filter(|p| (p.lexical_inject_rate - 1.0).abs() < 1e-9)
            .map(|p| p.fused_relative_floor)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            chosen.is_finite(),
            "some floor keeps full lexical recall while silencing off-topic"
        );
        eprintln!(
            "\n{}",
            render_floor_markdown(&points, "Relative-floor calibration (synthetic)")
        );
        eprintln!("Chosen relative-floor cutoff (largest lossless): {chosen:.2}");
    }
}
