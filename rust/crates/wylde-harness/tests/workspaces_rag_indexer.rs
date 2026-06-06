//! End-to-end workspace file-RAG indexer test with a deterministic mock
//! `wylde-ollama` embed pipe.
//!
//! Drives the REAL indexer (`reindex_full` / `reindex_delta` /
//! `search::query`) over a tmpdir of fake `.md` files, against a synthetic
//! `ollama.embed` backend that returns a fixed-vocabulary bag-of-words
//! vector per text. Because the same deterministic embedder scores both
//! the chunks (at index time) and the query (at search time), the ranking
//! is fully reproducible with no live infrastructure.
//!
//! Covers the indexer's contract end-to-end:
//!
//! * index 5 `.md` files, query, assert the topically-matching file ranks
//!   first; then
//! * mutate one file, **delta-reindex**, query again, assert the new
//!   content surfaces in the results.
//!
//! Windows-only — IPC uses named pipes. The pure ranking / chunking /
//! delta-selection logic is additionally unit-tested in the lib.

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::OnceCell;
use wylde_harness::workspaces::rag::indexer::{self, search};
use wylde_harness::workspaces::registry;
use wylde_shared::ipc;

/// Tiny fixed vocabulary — each dimension is the case-insensitive
/// occurrence count of one keyword. Distinct enough that each fake file
/// occupies its own corner of the space.
const VOCAB: &[&str] = &[
    "rust",
    "borrow",
    "python",
    "recipe",
    "planet",
    "telescope",
    "guitar",
    "chord",
];

fn embed_text(text: &str) -> Vec<f64> {
    let lower = text.to_lowercase();
    VOCAB
        .iter()
        .map(|kw| lower.matches(kw).count() as f64)
        .collect()
}

struct Mock {
    _server: Arc<ipc::PipeServer>,
    _data_dir: TempDir,
}

/// Stand up the mock embed pipe + a per-binary data dir exactly once.
/// `WYLDE_EMBED_*_DIM` are pinned to the vocab length so the embedder's
/// shape validation accepts our small vectors.
async fn mock() -> &'static Mock {
    static M: OnceCell<Mock> = OnceCell::const_new();
    M.get_or_init(|| async {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let service = format!("ollama-embed-mock-{suffix}");
        std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &service);
        std::env::set_var("WYLDE_EMBED_NATIVE_DIM", VOCAB.len().to_string());
        std::env::set_var("WYLDE_EMBED_DIM", VOCAB.len().to_string());

        let data_dir = TempDir::new().expect("data dir");
        std::env::set_var("WYLDE_DATA_DIR", data_dir.path());

        ipc::register_action("ollama.embed", move |payload: Value| async move {
            let inputs = payload
                .get("input")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let embeddings: Vec<Value> = inputs
                .iter()
                .map(|t| json!(embed_text(t.as_str().unwrap_or(""))))
                .collect();
            ipc::Reply::ok(json!({ "embeddings": embeddings }))
        });

        let server = Arc::new(ipc::PipeServer::new(&service));
        let server_clone = Arc::clone(&server);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            let _ = rt.block_on(server_clone.accept_loop());
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        Mock {
            _server: server,
            _data_dir: data_dir,
        }
    })
    .await
}

fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

#[tokio::test]
async fn index_query_then_delta_reindex_surfaces_new_content() {
    let _m = mock().await;

    // ── 5 fake .md files, each its own topic ────────────────────────
    let folder = TempDir::new().unwrap();
    let root = folder.path();
    std::fs::write(
        root.join("rust.md"),
        "# Rust\nRust ownership and the borrow checker. The borrow rules in rust.",
    )
    .unwrap();
    std::fs::write(
        root.join("python.md"),
        "# Python\nThe python interpreter and the python GIL.",
    )
    .unwrap();
    std::fs::write(
        root.join("cooking.md"),
        "# Cooking\nA recipe with tomato. Another recipe idea.",
    )
    .unwrap();
    std::fs::write(
        root.join("astronomy.md"),
        "# Astronomy\nA planet orbits its star. Point the telescope at the planet.",
    )
    .unwrap();
    std::fs::write(
        root.join("music.md"),
        "# Music\nA guitar chord and a melody. Strum the guitar chord.",
    )
    .unwrap();

    // ── full index ──────────────────────────────────────────────────
    let def = registry::create(&root.to_string_lossy(), Some("RagTest"));
    let outcome = indexer::reindex_full(&def).await;
    assert!(outcome.error.is_none(), "index failed: {:?}", outcome.error);
    assert_eq!(outcome.file_count, 5, "all 5 files indexed");
    assert!(outcome.chunk_count >= 5);

    // Status reflects a finished index.
    let status = indexer::status(&def.id);
    assert!(!status.indexing);
    assert_eq!(status.file_count, 5);
    assert!(status.last_indexed_at > 0.0);

    // ── query: rust borrow checker → rust.md ranks first ────────────
    let hits = search::query(&def.id, "how does the rust borrow checker work", 3).await;
    assert!(!hits.is_empty(), "expected ranked hits");
    assert!(
        norm(&hits[0].file_path).ends_with("rust.md"),
        "rust.md should rank first, got {}",
        hits[0].file_path
    );
    assert!(hits[0].score > 0.0);
    // Line range is populated + sane.
    assert!(hits[0].line_range[0] >= 1 && hits[0].line_range[1] >= hits[0].line_range[0]);

    // A different topic resolves to its own file.
    let cook = search::query(&def.id, "a tomato recipe", 1).await;
    assert!(norm(&cook[0].file_path).ends_with("cooking.md"));

    // Before mutation, astronomy.md is NOT about guitars.
    let before = search::query(&def.id, "guitar chord", 5).await;
    let astro_before = before
        .iter()
        .find(|h| norm(&h.file_path).ends_with("astronomy.md"));
    assert!(
        astro_before.map(|h| h.score).unwrap_or(0.0) < 0.01,
        "astronomy.md should not match guitar before mutation"
    );

    // ── mutate one file → delta reindex → new content surfaces ──────
    let astro = root.join("astronomy.md");
    let new_body = "# Astronomy Jam\nNow about a guitar chord and another guitar chord.";
    std::fs::write(&astro, new_body).unwrap();
    // Force a strictly-newer mtime so the delta selector re-embeds it,
    // independent of filesystem timestamp granularity.
    std::fs::File::options()
        .write(true)
        .open(&astro)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(120))
        .unwrap();

    let delta = indexer::reindex_delta(&def).await;
    assert!(delta.error.is_none(), "delta failed: {:?}", delta.error);
    assert_eq!(delta.file_count, 5, "still 5 files after delta");

    // The mutated file now matches the guitar query AND carries the new
    // content — proof the delta re-embedded it rather than serving stale
    // chunks.
    let after = search::query(&def.id, "guitar chord", 5).await;
    let astro_hit = after
        .iter()
        .find(|h| norm(&h.file_path).ends_with("astronomy.md"))
        .expect("mutated astronomy.md must now appear for a guitar query");
    assert!(
        astro_hit.content.contains("guitar chord"),
        "delta must surface the NEW content, got: {}",
        astro_hit.content
    );
    assert!(astro_hit.score > 0.0);
}

/// Live-backend smoke test of the full real path — `#[ignore]`-marked
/// because it needs a running `wylde-ollama` with the embed model pulled,
/// same convention as `tests/embed_live.rs`. Run with:
/// `cargo test -p wylde-harness --test workspaces_rag_indexer -- --ignored`.
#[tokio::test]
#[ignore = "requires live wylde-ollama with the embed model pulled"]
async fn live_index_and_query_round_trip() {
    let data_dir = TempDir::new().unwrap();
    std::env::set_var("WYLDE_DATA_DIR", data_dir.path());
    let folder = TempDir::new().unwrap();
    std::fs::write(
        folder.path().join("notes.md"),
        "The Wylde harness embeds workspace files with nomic-embed-text.",
    )
    .unwrap();
    std::fs::write(
        folder.path().join("other.md"),
        "Completely unrelated content about gardening and soil.",
    )
    .unwrap();

    let def = registry::create(&folder.path().to_string_lossy(), None);
    let outcome = indexer::reindex_full(&def).await;
    assert!(outcome.error.is_none(), "live index failed: {:?}", outcome.error);

    let hits = search::query(&def.id, "how are workspace files embedded?", 2).await;
    assert!(!hits.is_empty());
    assert!(norm(&hits[0].file_path).ends_with("notes.md"));
}
