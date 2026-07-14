//! Live lexical-BM25 + RRF eval driver — the impure half (lexical-bm25 plan L7 /
//! §5). `#[ignore]`d so it never runs in normal `cargo test` / CI; run it by
//! hand against the live index + a running Ollama:
//!
//! ```text
//! cargo test -p wylde-workspaces --test lexical_eval -- --ignored --nocapture
//! ```
//!
//! It reads the **live** `chunks.jsonl` (real `nomic-embed-text` vectors) into an
//! in-memory corpus, builds a **scratch** tantivy index under a temp
//! `WYLDE_DATA_DIR` (so the live data is never touched), embeds the gold queries
//! against the running Ollama (`/api/embed`, no task-prefix — exactly the RAG
//! path), runs the pure [`wylde_workspaces::rag::lexical_eval`] harness, and
//! writes `outputs/lexical-bm25-eval-results.md`.
//!
//! The shipped library carries no HTTP client; `reqwest` is a **dev**-dependency
//! used only here (mirrors `wylde-concept-routing/tests/live_eval.rs`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use wylde_workspaces::rag::lexical_eval::{
    build_corpus_index, calibrate_floor, render_floor_markdown, render_report_markdown,
    render_sweep_markdown, run_eval, sweep_rrf, Arm, EvalChunk, LexClass, LexGoldCase,
};
use wylde_workspaces::rag::LexicalConfig;

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

/// The single workspace dir holding an index.
fn workspace_dir() -> PathBuf {
    let dir = live_data_dir().join("workspaces");
    std::fs::read_dir(&dir)
        .expect("workspaces dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("index/chunks.jsonl").exists())
        .expect("a workspace with index/chunks.jsonl")
}

#[derive(Deserialize)]
struct RawChunk {
    id: String,
    path: String,
    #[serde(default)]
    content: String,
    vector: Vec<f32>,
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
        out.push(EvalChunk {
            id: c.id,
            path: c.path,
            content: c.content,
            vector: c.vector,
        });
    }
    out
}

/// Embed one query against the running Ollama, exactly as the RAG path does (no
/// task-prefix). `None` on any failure.
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

/// The DRAFT gold set — grounded in real Wylde source (Aaron VETs + extends, same
/// disclaimer as the routing gold set). The **lexical class** is exact
/// identifiers / error codes / rare tokens the embedder blurs (where the BM25 arm
/// is expected to win); the **semantic class** is topical queries dense already
/// nails (the no-regression guardrail); the **off-topic** cases ground nothing
/// (the cutoff guard).
fn gold() -> Vec<LexGoldCase> {
    let lex = |id: &str, q: &str, files: &[&str]| LexGoldCase {
        id: id.into(),
        query: q.into(),
        relevant_files: files.iter().map(|s| s.to_string()).collect(),
        class: LexClass::Lexical,
    };
    let sem = |id: &str, q: &str, files: &[&str]| LexGoldCase {
        id: id.into(),
        query: q.into(),
        relevant_files: files.iter().map(|s| s.to_string()).collect(),
        class: LexClass::Semantic,
    };
    let off = |id: &str, q: &str| LexGoldCase {
        id: id.into(),
        query: q.into(),
        relevant_files: vec![],
        class: LexClass::OffTopic,
    };
    vec![
        // ── Lexical class — exact tokens the embedder can't recover ──
        lex("anchor_boost_cap", "ANCHOR_BOOST_CAP", &["rag/indexer/search.rs"]),
        lex("embed_dim", "WYLDE_EMBED_DIM", &["common.rs"]),
        lex("rdcw", "ReadDirectoryChangesW", &["Cargo.toml"]),
        lex("nucleo", "nucleo-matcher", &["wylde-workspaces/Cargo.toml"]),
        lex("compose_fn", "compose_retrieval_query", &["turn/context_gather.rs"]),
        lex("oserr32", "os error 32", &["wylde-prebuild-guard"]),
        lex("min_abs", "MIN_ABSOLUTE_SCORE", &["rag/indexer/search.rs"]),
        lex("rrf_fuse", "rrf_k fuse", &["rag/indexer/fuse.rs"]),
        // ── Semantic class — topical queries dense should nail ──
        sem("watcher", "how does the file watcher debounce save events", &["watcher"]),
        sem("embed", "how are workspace chunks embedded for retrieval", &["embeddings.rs"]),
        sem("anchorbias", "how does anchor-biased retrieval boost the defining file", &["rag/indexer/search.rs"]),
        sem("manifest", "how does the content-hash manifest decide what to re-embed", &["rag/indexer/manifest.rs"]),
        sem("mmr", "how are near-duplicate chunks pruned from results", &["rag/indexer/search.rs"]),
        // ── Off-topic — should ground nothing ──
        off("pizza", "best pizza dough hydration ratio"),
        off("weather", "will it rain tomorrow in seattle"),
    ]
}

#[test]
#[ignore = "live: reads the persisted index + needs a running Ollama"]
fn run_full_eval() {
    let ws_dir = workspace_dir();
    eprintln!("workspace: {}", ws_dir.display());

    let t0 = std::time::Instant::now();
    let chunks = load_chunks(&ws_dir);
    eprintln!("loaded {} chunks in {:?}", chunks.len(), t0.elapsed());
    assert!(!chunks.is_empty(), "no chunks — is the workspace indexed?");

    // Build the scratch tantivy index under a temp data dir (never touch live).
    let scratch = tempfile::tempdir().expect("scratch dir");
    std::env::set_var("WYLDE_DATA_DIR", scratch.path());
    let ws = "lexeval";
    let t1 = std::time::Instant::now();
    build_corpus_index(ws, &chunks);
    eprintln!("built scratch BM25 index in {:?} (no embedder)", t1.elapsed());

    let gold = gold();
    let (lexn, semn, offn) = gold.iter().fold((0, 0, 0), |(l, s, o), c| match c.class {
        LexClass::Lexical => (l + 1, s, o),
        LexClass::Semantic => (l, s + 1, o),
        LexClass::OffTopic => (l, s, o + 1),
    });
    eprintln!("gold: {} cases (lexical {lexn} / semantic {semn} / off-topic {offn})", gold.len());

    // Embed every gold query against the live Ollama.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client");
    let mut query_vecs: HashMap<String, Vec<f32>> = HashMap::new();
    for case in &gold {
        if let Some(v) = embed_query(&client, &case.query) {
            query_vecs.insert(case.id.clone(), v);
        } else {
            eprintln!("  embed FAILED for {}", case.id);
        }
    }
    assert!(
        !query_vecs.is_empty(),
        "no query embeddings — is Ollama up on :11434 with {EMBED_MODEL}?"
    );
    eprintln!("embedded {}/{} gold queries", query_vecs.len(), gold.len());

    let base_cfg = LexicalConfig {
        enabled: true,
        ..LexicalConfig::default()
    };

    // The headline report: dense / lexical / fused × {lexical, semantic}.
    let report = run_eval(ws, &chunks, &gold, &query_vecs, K, &base_cfg);

    // RRF-parameter sweep (the win axis vs the guardrail).
    let sweep = sweep_rrf(
        ws,
        &chunks,
        &gold,
        &query_vecs,
        K,
        &base_cfg,
        &[
            (60.0, 1.0, 1.0),
            (60.0, 1.0, 1.5),
            (60.0, 1.5, 1.0),
            (30.0, 1.0, 1.0),
            (10.0, 1.0, 1.0),
        ],
    );

    // Cutoff-floor calibration (lands fused_relative_floor).
    let floors = calibrate_floor(
        ws,
        &chunks,
        &gold,
        &query_vecs,
        K,
        &base_cfg,
        &[0.3, 0.5, 0.6, 0.7, 0.8, 0.9],
    );

    // ── Emit the report ──
    let mut md = String::new();
    md.push_str("# Lexical (BM25) + RRF Fusion — Live Eval Results\n\n");
    md.push_str(&format!(
        "**Generated by** `tests/lexical_eval.rs::run_full_eval` (run by hand). \
         **Measured live:** {} chunks (real `nomic-embed-text` vectors), {} gold \
         queries embedded against the running Ollama; the BM25 index is rebuilt \
         from the live chunk set in a scratch dir (no embedder). **k = {}.**\n\n\
         The arms rank the full candidate set top-k (no production dynamic-k / \
         MMR) so recall@k isolates *ranking* quality; the cutoff itself is the \
         floor-calibration table below. The gold set is a DRAFT grounded in real \
         Wylde source — Aaron vets + extends.\n\n",
        chunks.len(),
        query_vecs.len(),
        K,
    ));
    md.push_str(&render_report_markdown(&report, "Arms × class — recall@k / nDCG@k / precision@k"));
    md.push('\n');
    md.push_str(&render_sweep_markdown(&sweep, "RRF parameter sweep (fused arm)"));
    md.push('\n');
    md.push_str(&render_floor_markdown(&floors, "Relative-floor cutoff calibration"));

    let out = std::env::var_os("WYLDE_EVAL_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../outputs/lexical-bm25-eval-results.md")
        });
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, &md).expect("write results md");
    eprintln!("wrote {}", out.display());

    println!("\n========== LEXICAL-BM25 EVAL SUMMARY (k={K}) ==========");
    println!("{}", render_report_markdown(&report, "Arms × class"));
    println!("{}", render_sweep_markdown(&sweep, "RRF sweep"));
    println!("{}", render_floor_markdown(&floors, "Floor calibration"));

    // Sanity gate so a regression in the live numbers is loud.
    let dense_lex = report.agg(Arm::Dense, LexClass::Lexical).map(|a| a.recall).unwrap_or(0.0);
    let fused_lex = report.agg(Arm::Fused, LexClass::Lexical).map(|a| a.recall).unwrap_or(0.0);
    let dense_sem = report.agg(Arm::Dense, LexClass::Semantic).map(|a| a.recall).unwrap_or(0.0);
    let fused_sem = report.agg(Arm::Fused, LexClass::Semantic).map(|a| a.recall).unwrap_or(0.0);
    eprintln!("lexical recall: dense {dense_lex:.3} → fused {fused_lex:.3}");
    eprintln!("semantic recall (guardrail): dense {dense_sem:.3} → fused {fused_sem:.3}");
    assert!(fused_lex >= dense_lex, "fused must not lose lexical-class recall");
    assert!(fused_sem + 0.001 >= dense_sem, "fused must not hurt the semantic guardrail");
}
