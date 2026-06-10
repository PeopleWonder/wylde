//! Live-backend embedder integration test.
//!
//! `#[ignore]`-marked because it requires a running `wylde-ollama` pipe
//! with the embedding model pulled — neither precondition is guaranteed
//! on a clean CI box. Run locally with:
//!
//! ```text
//! cargo test -p wylde-harness --test embed_live -- --ignored
//! ```
//!
//! Same mark the Wylde user used for the Phase 7.B-1 memgraph integration test
//! (one ignored test per slice that needs live infra). The pure shape
//! logic is covered by the lib's `memory::embeddings::tests` module.

#![cfg(windows)]

use wylde_harness::memory::embeddings;
use wylde_harness::memory::long_term;

#[tokio::test]
#[ignore = "requires live wylde-ollama with the embed model pulled"]
async fn embed_round_trip_against_live_wylde_ollama() {
    let v = embeddings::embed_one("hello, wylde".to_owned())
        .await
        .expect("live embed must succeed");
    assert!(!v.is_empty(), "embedder returned an empty vector");
    // Default model is 768-dim (nomic-embed-text); the Matryoshka
    // truncation only kicks in when WYLDE_EMBED_DIM is smaller.
    let expected = std::env::var("WYLDE_EMBED_DIM")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(768);
    assert_eq!(v.len(), expected, "vector dim mismatch");
}

#[tokio::test]
#[ignore = "requires live wylde-ollama with the embed model pulled"]
async fn text_search_round_trip_against_live_wylde_ollama() {
    // Save a record so there's at least one row to find.
    let saved = long_term::save(
        "embed-live integration test record",
        "test",
        Some(5.0),
        Vec::new(),
        // No precomputed vector — the next save+search round-trip
        // relies on the embedder doing both directions.
        Some(
            embeddings::embed_one("embed-live integration test record".to_owned())
                .await
                .expect("embed save body"),
        ),
    )
    .expect("save record");

    let hits = long_term::text_search("embed-live integration", 5, None)
        .await
        .expect("text_search succeeds");
    assert!(
        !hits.is_empty(),
        "text_search must surface the seeded record"
    );
    assert!(
        hits.iter().any(|h| h.id == saved.id),
        "seeded record not in hits"
    );
    long_term::delete(&saved.id);
}
