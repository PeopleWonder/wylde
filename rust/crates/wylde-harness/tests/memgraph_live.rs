//! Live Memgraph/Neo4j end-to-end integration test.
//!
//! This is the missing "actually exercised against a real DB" coverage
//! for [`wylde_harness::memory::memgraph::bolt::BoltClient`]. Every other
//! memgraph test is pure (coercion / depth-clamp / error-path) and never
//! opens a Bolt socket, so the write Cypher had never been proven to
//! land edges a reader can see.
//!
//! ## Ignored by default — needs a live DB
//!
//! It is `#[ignore]`d because it requires a Neo4j/Memgraph listening on
//! `bolt://127.0.0.1:7687` (auth disabled — the Wylde default). Run it
//! explicitly once a DB is up:
//!
//! ```text
//! cargo test -p wylde-harness --test memgraph_live -- --ignored --nocapture
//! ```
//!
//! Point it elsewhere with `GRAPH_BOLT_URL` / `GRAPH_USER` /
//! `GRAPH_PASSWORD` (same env knobs the production client reads). The
//! bundled dev DB lives at `Core/Memgraph/vendor/neo4j` (needs a JDK 21+
//! on `JAVA_HOME`).
//!
//! ## What it proves
//!
//! Against a live graph, writing then reading back:
//! * `upsert` lands Chunk + Entity nodes, `MENTIONED_IN` edges, and the
//!   typed Entity→Entity edges embedded in the chunks (`CALLS` /
//!   `IMPORTS`).
//! * `traverse` and `multihop` reach those chunks from a seed entity —
//!   i.e. the relational links are queryable, not just written.
//! * `relate` / `unrelate` add and remove typed edges, reflected in
//!   `stats.typed_relationships`.
//! * `upsert_edge` (the RAG-feedback / reflection weighted edge) writes
//!   without error.
//! * The workspace-memory save path's best-effort entity→graph write
//!   (`record_entities_best_effort` → `BoltClient::upsert`) lands a
//!   `MENTIONED_IN` edge end-to-end through the real
//!   `memory.workspace.save` handler.
//!
//! Everything is namespaced under `wylde_mglt_*` ids and torn down at
//! start + end, so the test is idempotent and leaves no cruft.

#![cfg(windows)]

use serde_json::json;

use wylde_harness::memory::memgraph::bolt::BoltClient;
use wylde_harness::memory::memgraph::client::{EntityPair, TraverseRequest};
use wylde_harness::memory::workspace::actions as ws_actions;

const WS: &str = "wylde_mglt_ws";

fn client() -> BoltClient {
    BoltClient::new()
}

/// Fail loudly (rather than silently pass) when the ignored test is run
/// without a reachable DB — the whole point is to exercise a live one.
async fn require_live(c: &BoltClient) {
    let reply = c.health().await;
    assert!(
        reply.ok,
        "Neo4j/Memgraph is not reachable at {} — start the bundled dev DB \
         (Core/Memgraph/vendor/neo4j, JDK 21+) or set GRAPH_BOLT_URL before \
         running this ignored test. health() said: {:?}",
        c.config().uri,
        reply.error
    );
}

fn count(reply: &wylde_shared::ipc::Reply, key: &str) -> i64 {
    assert!(reply.ok, "stats failed: {:?}", reply.error);
    reply.data.get(key).and_then(serde_json::Value::as_i64).unwrap_or(-1)
}

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687"]
async fn upsert_traverse_relate_round_trip_lands_and_reads_edges() {
    let c = client();
    require_live(&c).await;

    // Clean slate (idempotent) + schema.
    let _ = c.delete_workspace(WS).await;
    assert!(c.ensure_schema().await.ok, "ensure_schema failed");

    let before = c.stats().await;
    let mentions_before = count(&before, "mentions");
    let typed_before = count(&before, "typed_relationships");

    // ── upsert: chunks + entities + MENTIONED_IN + typed Entity edges ──
    let chunks = vec![
        json!({
            "id": "wylde_mglt_c1",
            "path": "wylde_mglt/a.py",
            "symbol": "alpha",
            "language": "python",
            "workspace": WS,
            "entities": ["wylde_mglt_alpha", "wylde_mglt_beta"],
            "relationships": [
                {"type": "CALLS", "source": "wylde_mglt_alpha", "target": "wylde_mglt_beta"}
            ],
        }),
        json!({
            "id": "wylde_mglt_c2",
            "path": "wylde_mglt/b.py",
            "symbol": "beta",
            "language": "python",
            "workspace": WS,
            "entities": ["wylde_mglt_beta", "wylde_mglt_gamma"],
            "relationships": [
                {"type": "IMPORTS", "source": "wylde_mglt_beta", "target": "wylde_mglt_gamma"}
            ],
        }),
    ];
    let up = c.upsert(chunks).await;
    assert!(up.ok, "upsert failed: {:?}", up.error);
    assert_eq!(up.data["count"], 2);

    // stats reflect the new mentions + typed edges.
    let after = c.stats().await;
    assert!(
        count(&after, "mentions") >= mentions_before + 4,
        "expected >=4 new MENTIONED_IN edges (c1: alpha,beta; c2: beta,gamma)"
    );
    assert!(
        count(&after, "typed_relationships") >= typed_before + 2,
        "expected >=2 new typed edges (CALLS + IMPORTS)"
    );

    // ── traverse: reach the chunks from a seed entity ─────────────────
    let trav = c
        .traverse(TraverseRequest {
            entities: vec!["wylde_mglt_alpha".into()],
            max_hops: 3,
            limit: 10,
            workspace: Some(WS.into()),
            decay_alpha: None,
            rel_depths: None,
        })
        .await;
    assert!(trav.ok, "traverse failed: {:?}", trav.error);
    let trav_ids: Vec<String> = trav.data["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        trav_ids.iter().any(|id| id == "wylde_mglt_c1"),
        "traverse from alpha should reach c1 via CALLS; got {trav_ids:?}"
    );

    // ── multihop: expand co-mentioned entities, collect chunks ────────
    let multi = c
        .multihop(vec!["wylde_mglt_alpha".into()], 2, 20)
        .await;
    assert!(multi.ok, "multihop failed: {:?}", multi.error);
    let expanded: Vec<String> = multi.data["expanded_entities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str().map(str::to_owned))
        .collect();
    assert!(
        expanded.iter().any(|e| e == "wylde_mglt_beta"),
        "multihop from alpha should reach the co-mentioned beta; got {expanded:?}"
    );
    assert!(
        !multi.data["chunks"].as_array().unwrap().is_empty(),
        "multihop should surface chunks"
    );

    // ── relate / unrelate: a vocab edge, verified via stats ───────────
    let rel = c
        .relate(
            "INHERITS",
            vec![EntityPair::new("wylde_mglt_gamma", "wylde_mglt_alpha")],
        )
        .await;
    assert!(rel.ok, "relate failed: {:?}", rel.error);
    assert_eq!(rel.data["written"], 1);
    let typed_after_relate = count(&c.stats().await, "typed_relationships");
    assert!(
        typed_after_relate >= typed_before + 3,
        "INHERITS edge should be counted in typed_relationships"
    );

    let unrel = c
        .unrelate(
            "INHERITS",
            vec![EntityPair::new("wylde_mglt_gamma", "wylde_mglt_alpha")],
        )
        .await;
    assert!(unrel.ok, "unrelate failed: {:?}", unrel.error);
    assert_eq!(unrel.data["deleted"], 1);

    // ── upsert_edge: the weighted RAG-feedback / reflection edge ──────
    let edge = c
        .upsert_edge("wylde_mglt_alpha", "RELATED", "wylde_mglt_gamma", 2.5)
        .await;
    assert!(edge.ok, "upsert_edge failed: {:?}", edge.error);
    // Idempotent re-apply must also succeed (ON MATCH weight bump path).
    let edge2 = c
        .upsert_edge("wylde_mglt_alpha", "RELATED", "wylde_mglt_gamma", 2.5)
        .await;
    assert!(edge2.ok, "upsert_edge re-apply failed: {:?}", edge2.error);

    // ── teardown ──────────────────────────────────────────────────────
    let del = c.delete_workspace(WS).await;
    assert!(del.ok, "delete_workspace failed: {:?}", del.error);
}

/// End-to-end: the real `memory.workspace.save` handler's best-effort
/// entity→graph write lands a `MENTIONED_IN` edge. The graph write is
/// fire-and-forget on a spawned task, so we poll `stats.mentions` for a
/// bounded window rather than assume it's synchronous.
#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687"]
async fn workspace_memory_save_lands_entity_graph_edge() {
    let c = client();
    require_live(&c).await;
    let _ = c.delete_workspace(WS).await;
    assert!(c.ensure_schema().await.ok);

    // The save handler needs a data dir for the JSON store.
    let tmp = std::env::temp_dir().join("wylde_mglt_datadir");
    std::env::set_var("WYLDE_DATA_DIR", &tmp);

    let mentions_before = count(&c.stats().await, "mentions");

    let reply = ws_actions::handle_save(json!({
        "workspace_id": WS,
        "body": "the watcher polls the outputs directory",
        "entities": ["wylde_mglt_watcher", "wylde_mglt_outputs"],
    }))
    .await;
    assert!(reply.ok, "handle_save failed: {:?}", reply.error);

    // Poll for the fire-and-forget graph write to land (bounded ~5s).
    let mut landed = false;
    for _ in 0..50 {
        if count(&c.stats().await, "mentions") >= mentions_before + 2 {
            landed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        landed,
        "workspace save's best-effort MENTIONED_IN edges never appeared in the graph"
    );

    let del = c.delete_workspace(WS).await;
    assert!(del.ok);
    std::env::remove_var("WYLDE_DATA_DIR");
}
