//! Live R4 eval driver — the impure half of the eval (concept-routing plan
//! §6.4). `#[ignore]`d so it never runs in normal `cargo test` / CI; run it by
//! hand:
//!
//! ```text
//! cargo test -p wylde-concept-routing --test live_eval -- --ignored --nocapture
//! ```
//!
//! It builds an [`EvalCorpus`] from the **live** persisted workspace index
//! (`chunks.jsonl`, real `nomic-embed-text` vectors) + the **decrypted**
//! concept store, embeds the gold queries against the **running** Ollama
//! (`/api/embed`, no task-prefix — exactly the RAG path, see
//! `wylde-workspaces/src/embeddings.rs`), runs the pure
//! [`wylde_concept_routing::eval`] harness, and writes
//! `outputs/concept-routing-r4-eval-results.md`.
//!
//! Everything novel is in the pure crate; this file is only I/O (read the
//! index, decrypt the concepts, HTTP the embeds) — it carries `reqwest` as a
//! **dev**-dependency so the shipped library still depends on `wylde-shared`
//! only (the isolation contract holds).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use wylde_concept_routing::config::{RelationParams, RoutingConfig};
use wylde_concept_routing::eval::{
    render_report_markdown, render_sweep_markdown, run_eval, sweep_abs_threshold, Arm, CaseKind,
    EvalChunk, EvalConcept, EvalCorpus, GoldSet, RelationMode,
};
use wylde_concept_routing::eval::corpus::normalize_path;
use wylde_concept_routing::relations::{NodeRef, Relation, RelationGraph, RelationKind};

const OLLAMA_URL: &str = "http://localhost:11434/api/embed";
const EMBED_MODEL: &str = "nomic-embed-text";
const K: usize = 10;

fn live_data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_EVAL_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(root) = std::env::var_os("WYLDE_ROOT") {
        return PathBuf::from(root).join(".wylde").join("data");
    }
    PathBuf::from(r"C:\Users\aaron\Documents\Obsidian Vault\Wylde-release\.wylde\data")
}

/// The single workspace dir holding a concepts.json + an index.
fn workspace_dir() -> PathBuf {
    let dir = live_data_dir().join("workspaces");
    std::fs::read_dir(&dir)
        .expect("workspaces dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("concepts.json").exists() && p.join("index/chunks.jsonl").exists())
        .expect("a workspace with concepts.json + index/chunks.jsonl")
}

// ── Raw on-disk shapes ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawChunk {
    path: String,
    #[serde(default)]
    content: String,
    vector: Vec<f32>,
}

#[derive(Deserialize)]
struct RawConcept {
    id: String,
    label: String,
    #[serde(default)]
    member_files: Vec<String>,
    #[serde(default)]
    described_by: Vec<String>,
    #[serde(default)]
    centroid: Option<Vec<f32>>,
}

fn load_chunks(ws: &Path) -> Vec<EvalChunk> {
    let f = std::fs::File::open(ws.join("index/chunks.jsonl")).expect("open chunks.jsonl");
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(c) = serde_json::from_str::<RawChunk>(&line) else {
            continue;
        };
        if c.vector.is_empty() {
            continue;
        }
        // ~4 chars/token (the budget convention used across the harness).
        let tokens = (c.content.chars().count() / 4).max(1);
        out.push(EvalChunk {
            path: normalize_path(&c.path),
            vector: c.vector,
            tokens,
        });
    }
    out
}

fn load_concepts(ws: &Path) -> Vec<EvalConcept> {
    let raw = wylde_shared::encryption::read_to_string_at_rest(&ws.join("concepts.json"))
        .expect("decrypt concepts.json");
    let concepts: Vec<RawConcept> = serde_json::from_str(&raw).expect("parse concepts.json");
    concepts
        .into_iter()
        .filter_map(|c| {
            let centroid = c.centroid?;
            if centroid.is_empty() {
                return None;
            }
            Some(EvalConcept {
                id: c.id,
                label: c.label,
                centroid,
                member_files: c.member_files.iter().map(|f| normalize_path(f)).collect(),
                described_by: c.described_by,
            })
        })
        .collect()
}

/// Embed one query against the running Ollama, exactly as the RAG path does
/// (no task-prefix). Returns `None` on any failure (unreachable / bad shape).
fn embed_query(client: &reqwest::blocking::Client, text: &str) -> Option<Vec<f32>> {
    let body = serde_json::json!({ "model": EMBED_MODEL, "input": [text] });
    let resp = client.post(OLLAMA_URL).json(&body).send().ok()?;
    if !resp.status().is_success() {
        eprintln!("embed HTTP {}", resp.status());
        return None;
    }
    let v: serde_json::Value = resp.json().ok()?;
    let arr = v.get("embeddings")?.as_array()?;
    let first = arr.first()?.as_array()?;
    let vec: Vec<f32> = first.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
    (!vec.is_empty()).then_some(vec)
}

/// Author the relation fixture for the conflation/dependency gold cases over the
/// live (coarse) concepts: a `Negative` edge between the concept(s) covering a
/// conflation case's `relevant_files` and those covering its `avoid_files`, and
/// a `Dependency` edge for the dependency cases. Self-edges (when the same
/// coarse concept covers both sides) are skipped — that is itself a limitation
/// the report records.
fn author_relations(corpus: &EvalCorpus, gold: &GoldSet) -> (RelationGraph, usize, usize) {
    let mut rels: Vec<Relation> = Vec::new();
    let mut authored = 0usize;
    let mut skipped_self = 0usize;
    let add = |a: &[String], b: &[String], kind: RelationKind, rels: &mut Vec<Relation>| {
        let from = corpus.concepts_covering(a);
        let to = corpus.concepts_covering(b);
        let mut any = false;
        for f in &from {
            for t in &to {
                if f == t {
                    continue; // same coarse concept covers both → meaningless
                }
                any = true;
                rels.push(Relation::normalized(
                    NodeRef::concept(f),
                    NodeRef::concept(t),
                    kind,
                    None,
                ));
            }
        }
        any
    };
    for case in &gold.cases {
        match case.kind {
            CaseKind::Conflation => {
                if add(&case.relevant_files, &case.avoid_files, RelationKind::Negative, &mut rels) {
                    authored += 1;
                } else {
                    skipped_self += 1;
                }
            }
            CaseKind::Dependency => {
                if add(
                    &case.relevant_files,
                    &case.dependency_files,
                    RelationKind::Dependency,
                    &mut rels,
                ) {
                    authored += 1;
                } else {
                    skipped_self += 1;
                }
            }
            CaseKind::Easy => {}
        }
    }
    (RelationGraph { relations: rels }, authored, skipped_self)
}

#[test]
#[ignore = "live: reads the persisted, DPAPI-encrypted concepts store"]
fn dump_concepts() {
    let ws = workspace_dir();
    let concepts = load_concepts(&ws);
    println!("concepts with centroid: {}", concepts.len());
    for c in concepts.iter().take(8) {
        println!("  {} {:?} files={}", c.id, c.label, c.member_files.len());
    }
}

#[test]
#[ignore = "live: needs the persisted index + a running Ollama"]
fn run_full_eval() {
    let ws = workspace_dir();
    eprintln!("workspace: {}", ws.display());

    let t0 = std::time::Instant::now();
    let chunks = load_chunks(&ws);
    eprintln!("loaded {} chunks in {:?}", chunks.len(), t0.elapsed());
    let concepts = load_concepts(&ws);
    eprintln!("loaded {} centroid-bearing concepts", concepts.len());
    assert!(!chunks.is_empty(), "no chunks — is the workspace indexed?");
    assert!(!concepts.is_empty(), "no centroid concepts");

    let gold = GoldSet::embedded();
    let (easy, conf, dep) = gold.counts();
    eprintln!(
        "gold: {} cases (easy {easy} / conflation {conf} / dependency {dep})",
        gold.cases.len()
    );

    // Embed every gold query against the live Ollama.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client");
    let mut query_vecs: HashMap<String, Vec<f32>> = HashMap::new();
    let mut embed_fail = 0usize;
    for case in &gold.cases {
        match embed_query(&client, &case.query) {
            Some(v) => {
                query_vecs.insert(case.id.clone(), v);
            }
            None => {
                embed_fail += 1;
                eprintln!("  embed FAILED for {}", case.id);
            }
        }
    }
    eprintln!(
        "embedded {}/{} gold queries ({} failed)",
        query_vecs.len(),
        gold.cases.len(),
        embed_fail
    );
    assert!(
        !query_vecs.is_empty(),
        "no query embeddings — is Ollama up on :11434 with {EMBED_MODEL}?"
    );

    // Build the corpus + author the relation fixture for conflation/dependency.
    let mut corpus = EvalCorpus {
        chunks,
        concepts,
        relations: RelationGraph::empty(),
        vocab_terms: Vec::new(),
    };
    let (graph, authored, skipped_self) = author_relations(&corpus, &gold);
    corpus.relations = graph;
    eprintln!(
        "authored relations for {authored} cases ({skipped_self} skipped: one coarse concept covered both sides)",
    );

    // The R1 PROVISIONAL config (abs=0.50, pinned explicitly — the shipped
    // default is now the R4-calibrated 0.62) and the eval matrix.
    let provisional_cfg = RoutingConfig {
        enabled: true,
        abs_threshold: 0.50,
        ..RoutingConfig::default()
    };
    let arms = [Arm::Baseline, Arm::Augment, Arm::Replace];
    let modes = [RelationMode::SeedOnly, RelationMode::RelationsOn];

    let report_provisional =
        run_eval(&corpus, &gold, &query_vecs, &provisional_cfg, K, &arms, &modes);

    // Tuned config = the SHIPPED R4 default (abs raised to 0.62; relation
    // params left at the addendum defaults — on flat live cosines the absolute
    // floor binds, so relative_floor / dep_decay don't move the seed-only arms).
    let tuned_cfg = RoutingConfig {
        enabled: true,
        ..RoutingConfig::default()
    };
    let report_tuned = run_eval(&corpus, &gold, &query_vecs, &tuned_cfg, K, &arms, &modes);

    // Relations-aware experimental (dep_decay > relative_floor) — to show the
    // dependency-spread lever; recorded but NOT the shipped default.
    let rel_aware_cfg = RoutingConfig {
        enabled: true,
        relative_floor: 0.5,
        relation_params: RelationParams {
            dep_decay: 0.7,
            ..RelationParams::default()
        },
        ..RoutingConfig::default()
    };
    let report_rel_aware = run_eval(&corpus, &gold, &query_vecs, &rel_aware_cfg, K, &arms, &modes);

    // Threshold sweep for Replace (the calibration curve).
    let thresholds = [0.50f32, 0.55, 0.58, 0.60, 0.62, 0.64, 0.66, 0.70];
    let sweep = sweep_abs_threshold(
        &corpus,
        &gold,
        &query_vecs,
        &provisional_cfg,
        Arm::Replace,
        RelationMode::SeedOnly,
        K,
        &thresholds,
    );

    // ── Emit the report ──────────────────────────────────────────────────
    let mut md = String::new();
    md.push_str("# Concept-Routing R4 — Live Eval Results\n\n");
    md.push_str(&format!(
        "**Generated by** `tests/live_eval.rs::run_full_eval` (run by hand). \
         **Measured live:** {} chunks (real `nomic-embed-text` vectors), {} \
         centroid-bearing concepts, {} gold queries embedded against the running \
         Ollama. **Authored (not persisted live):** relation edges for the \
         conflation/dependency gold cases. **k = {}.**\n\n",
        corpus.chunks.len(),
        corpus.concepts.len(),
        query_vecs.len(),
        K,
    ));
    md.push_str(&format!(
        "> Index composition note: a large fraction of the live index is \
         build-artifact noise (rustdoc HTML under `target-dev/doc`, vendored \
         JDK, `deps/`), and the {} concepts are coarse auto-clusters with \
         generic labels (`Src`/`Ipc`/`Deps`). Both bound how well routing can \
         do live — see the verdict.\n\n",
        corpus.concepts.len()
    ));
    md.push_str(&render_report_markdown(
        &report_provisional,
        "Arms × ablation — PROVISIONAL config (abs=0.50, dep_decay=0.5)",
    ));
    md.push('\n');
    md.push_str(&render_report_markdown(
        &report_tuned,
        "Arms × ablation — SHIPPED R4 default (abs=0.62, addendum relation params)",
    ));
    md.push('\n');
    md.push_str(&render_report_markdown(
        &report_rel_aware,
        "Arms × ablation — relations-aware experimental (abs=0.62, relative_floor=0.5, dep_decay=0.7)",
    ));
    md.push('\n');
    md.push_str(&render_sweep_markdown(
        &sweep,
        "abs_threshold sweep — Replace / seed-only (the flat-cosine curve)",
    ));

    // Regenerable raw tables; the curated narrative + verdict lives in
    // `concept-routing-r4-eval-results.md` (hand-authored, not clobbered here).
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../outputs/concept-routing-r4-eval-tables.md");
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, &md).expect("write results md");
    eprintln!("wrote {}", out.display());

    // Console summary.
    println!("\n========== R4 EVAL SUMMARY (k={K}) ==========");
    println!("{}", render_report_markdown(&report_provisional, "PROVISIONAL (abs=0.50)"));
    println!("{}", render_report_markdown(&report_tuned, "SHIPPED DEFAULT (abs=0.62)"));
    println!(
        "{}",
        render_report_markdown(&report_rel_aware, "RELATIONS-AWARE (abs=0.62, dep_decay=0.7)")
    );
    println!("{}", render_sweep_markdown(&sweep, "SWEEP"));
}
