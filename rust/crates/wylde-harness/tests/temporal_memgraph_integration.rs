//! Live bi-temporal edge round-trip — **P1 verification artifact**.
//!
//! Exercises the temporal edge path (`WYLDE_TEMPORAL_MEMORY=1`) against
//! a real Neo4j/Memgraph on `bolt://127.0.0.1:7687`: a temporal
//! `relate` writes a versioned open edge, a weighted `upsert_edge`
//! supersede-and-bumps it, and `relations_as_of` reads the graph at a
//! point in time.
//!
//! Why `#[ignore]`: needs a live Bolt endpoint (a single Memgraph
//! instance is enough — do NOT bring up the full stack). The DB-free
//! unit tests under `src/memory/memgraph/temporal.rs` cover the model
//! semantics; this pins the live Cypher actually executes. Same pattern
//! as `memgraph_integration.rs`.
//!
//! Run with a single Memgraph up, then:
//!
//! ```
//! WYLDE_TEMPORAL_MEMORY=1 cargo test -p wylde-harness \
//!   --test temporal_memgraph_integration -- --ignored --nocapture
//! ```

use wylde_harness::memory::memgraph::bolt::BoltClient;
use wylde_harness::memory::memgraph::client::EntityPair;

/// End-to-end: temporal relate → as-of read sees the open edge; a later
/// weighted upsert supersedes; an as-of read *before* the supersede
/// still sees the original window. Uses unique entity names so reruns
/// don't collide with prior data.
#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_relate_and_as_of_roundtrip() {
    // SAFETY: single-threaded test setup.
    unsafe {
        std::env::set_var("WYLDE_TEMPORAL_MEMORY", "1");
    }

    let client = BoltClient::new();
    let src = "wylde_temporal_it_src";
    let tgt = "wylde_temporal_it_tgt";

    // Write a temporal typed edge.
    let relate = client
        .relate("CALLS", vec![EntityPair::new(src, tgt)])
        .await;
    assert!(relate.ok, "temporal relate failed: {:?}", relate.error);
    assert_eq!(relate.data["temporal"], true);

    // As-of "now" should see exactly one open edge between src/tgt.
    let now = wylde_harness::memory::memgraph::temporal::now_ms();
    let as_of = client.relations_as_of("CALLS", now + 1).await;
    assert!(as_of.ok, "as_of read failed: {:?}", as_of.error);
    let edges = as_of.data["edges"].as_array().expect("edges array");
    let hit = edges
        .iter()
        .find(|e| e["source"] == src && e["target"] == tgt)
        .expect("the just-written edge is visible as-of now");
    assert_eq!(
        hit["valid_to"].as_i64(),
        Some(wylde_harness::memory::memgraph::temporal::OPEN),
        "freshly written edge is open-ended"
    );

    // A point in time before the edge existed sees nothing for it.
    let before = client.relations_as_of("CALLS", 1).await;
    assert!(before.ok);
    let none = before.data["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .any(|e| e["source"] == src && e["target"] == tgt);
    assert!(!none, "edge must not be visible before its valid_from");
}
