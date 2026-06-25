//! Dev bench: measure real indexing throughput on a folder, into an ISOLATED
//! index dir, so the ETA in the live progress UI is calibrated against actual
//! numbers (and we can answer "how long does a full reindex take now?").
//!
//! It is deliberately non-disruptive to a running stack:
//!   * `WYLDE_DATA_DIR` is pinned to a throwaway temp dir, so it never touches
//!     the live index / registry.
//!   * `GRAPH_BOLT_URL` is pointed at a dead port (with a 1s connect timeout),
//!     so the graph-write half fails fast and NEVER writes into the shared
//!     Memgraph. We're measuring the vector pipeline (walk → chunk → embed →
//!     persist), which is what the progress bar tracks.
//!   * It uses the live `wylde-ollama` service for real embeds — the only
//!     shared dependency — at the production embed pacing, so the wall-clock is
//!     the real one a user would see.
//!
//! Run (from `rust/`):
//! ```text
//! cargo run -p wylde-workspaces --example index_bench --release -- "<folder>"
//! ```
//! Defaults the folder to the Wylde-release checkout if omitted.

use std::time::Instant;

use wylde_workspaces::rag::indexer::{self, walk};
use wylde_workspaces::registry;

fn main() {
    // ── Isolation: throwaway data dir + dead graph backend ──────────────────
    let tmp = std::env::temp_dir().join(format!("wylde-index-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    std::env::set_var("WYLDE_DATA_DIR", &tmp);
    // Never write into the shared Memgraph — dead port + fast-fail connect.
    std::env::set_var("GRAPH_BOLT_URL", "bolt://127.0.0.1:1");
    std::env::set_var("WYLDE_BOLT_CONNECT_TIMEOUT_SECS", "1");

    let folder = std::env::args().nth(1).unwrap_or_else(|| {
        r"C:\Users\aaron\Documents\Obsidian Vault\Wylde-release".to_owned()
    });
    println!("== index_bench ==");
    println!("folder    : {folder}");
    println!("data_dir  : {} (isolated)", tmp.display());
    println!();

    // ── Phase 0: walk + chunk only (no Ollama) ──────────────────────────────
    let t = Instant::now();
    let chunks = walk::walk_and_chunk(&folder);
    let walk_secs = t.elapsed().as_secs_f64();
    let mut files = std::collections::HashSet::new();
    for c in &chunks {
        files.insert(c.path.as_str());
    }
    println!(
        "walk+chunk: {:>6} chunks across {:>5} files in {:>6.2}s ({:.0} chunks/s)",
        chunks.len(),
        files.len(),
        walk_secs,
        chunks.len() as f64 / walk_secs.max(1e-6),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let def = registry::create(&folder, Some("index-bench"));

        // ── Phase 1: full from-scratch index (real embeds, production pacing) ─
        let t = Instant::now();
        let out = indexer::reindex_full(&def).await;
        let full_secs = t.elapsed().as_secs_f64();
        if let Some(e) = &out.error {
            eprintln!("FULL INDEX FAILED: {e}");
            eprintln!("(is wylde-ollama up with the embed model pulled?)");
            return;
        }
        println!(
            "FULL      : {:>6} chunks / {:>5} files in {:>7.1}s  ({:.1} chunks/s)",
            out.chunk_count,
            out.file_count,
            full_secs,
            out.chunk_count as f64 / full_secs.max(1e-6),
        );

        // ── Phase 2: incremental reindex (manifest reuse, no real changes) ───
        let t = Instant::now();
        let out2 = indexer::reindex(&def).await;
        let inc_secs = t.elapsed().as_secs_f64();
        println!(
            "INCREMENT : {:>6} chunks / {:>5} files in {:>7.2}s  (manifest reuse, 0 re-embeds)",
            out2.chunk_count, out2.file_count, inc_secs,
        );

        println!();
        println!("SUMMARY: from-scratch {full_secs:.1}s, incremental {inc_secs:.2}s");
    });

    // Best-effort cleanup of the throwaway index.
    let _ = std::fs::remove_dir_all(&tmp);
}
