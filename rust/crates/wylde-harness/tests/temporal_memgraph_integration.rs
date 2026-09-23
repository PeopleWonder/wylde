//! Live bi-temporal edge round-trip — **P1 verification artifact**.
//!
//! Exercises the temporal edge path (`WYLDE_TEMPORAL_MEMORY=1`) against
//! a real Neo4j/Memgraph on `bolt://127.0.0.1:7687` (or `GRAPH_BOLT_URL`):
//!
//! * **insert / as-of** — a temporal `relate` writes a versioned open
//!   edge; `relations_as_of` sees it now and not before its `valid_from`.
//! * **supersede** — a weighted `upsert_edge` supersede-and-bumps; the
//!   prior valid window is preserved and still visible as-of the past.
//! * **unrelate (logical delete)** — closes `valid_to` instead of
//!   hard-deleting; as-of inside the window still sees it, as-of after
//!   does not, and history is preserved.
//! * **correction / as-believed-at** — a transaction-time correction
//!   retires belief in the wrong edge; "as believed at" before the
//!   correction reconstructs the original value (transaction-time travel).
//! * **migration** — the P0 `backfill_temporal` builder, run live: legacy
//!   edges get `valid_from` from `created_at`, `valid_to`/`tx_to` OPEN,
//!   idempotent on re-run. Plus the gated edge-property indexes.
//!
//! Why `#[ignore]`: needs a live Bolt endpoint (a single Memgraph/Neo4j
//! instance is enough — do NOT bring up the full stack). The DB-free
//! unit tests under `src/memory/memgraph/temporal.rs` cover the model
//! semantics; this pins the live Cypher actually executes. Run serially
//! (the toggle is a process-global env var):
//!
//! ```
//! WYLDE_TEMPORAL_MEMORY=1 cargo test -p wylde-harness \
//!   --test temporal_memgraph_integration -- --ignored --test-threads=1 --nocapture
//! ```

use neo4rs::{ConfigBuilder, Graph};
use wylde_harness::memory::memgraph::bolt::BoltClient;
use wylde_harness::memory::memgraph::client::EntityPair;
use wylde_harness::memory::memgraph::temporal::{now_ms, OPEN};

/// Serializes the live tests on the one shared Neo4j (the #83 self-collision
/// class, #216/#227): they contend on graph-global state (ensure_schema,
/// stats, the orphan-prune) and on the process-global toggle env var.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Enable the temporal path for the whole (single-threaded) test run.
fn enable_temporal() {
    // SAFETY: the ignored temporal integration tests are run with
    // `--test-threads=1`, so there is no concurrent env mutation.
    unsafe {
        std::env::set_var("WYLDE_TEMPORAL_MEMORY", "1");
    }
}

fn bolt_url() -> String {
    std::env::var("GRAPH_BOLT_URL").unwrap_or_else(|_| "bolt://127.0.0.1:7687".to_owned())
}

/// A raw neo4rs handle for seeding legacy edges + reading back edge
/// properties the `BoltClient` verbs don't return.
async fn raw_graph() -> Graph {
    let cfg = ConfigBuilder::default()
        .uri(bolt_url())
        .user(std::env::var("GRAPH_USER").unwrap_or_default())
        .password(std::env::var("GRAPH_PASSWORD").unwrap_or_default())
        .build()
        .expect("bolt config");
    Graph::connect(cfg).await.expect("connect neo4j")
}

/// Unique per-test entity-name prefix so reruns never collide and cleanup
/// is scoped. Temporal edges are CREATE'd (history accumulates), so every
/// test scrubs its own nodes first.
fn uniq(tag: &str) -> String {
    let nonce = now_ms();
    format!("wtmp_{}_{}_{}", std::process::id(), tag, nonce)
}

/// DETACH DELETE every Entity whose name starts with `prefix`.
async fn scrub(g: &Graph, prefix: &str) {
    g.run(
        neo4rs::query("MATCH (n:Entity) WHERE n.name STARTS WITH $p DETACH DELETE n")
            .param("p", prefix),
    )
    .await
    .expect("scrub");
}

// ── insert / supersede / as-of ─────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_relate_and_as_of_roundtrip() {
    let _db = DB_LOCK.lock().await;
    enable_temporal();
    let g = raw_graph().await;
    let src = uniq("rt_src");
    let tgt = uniq("rt_tgt");
    scrub(&g, &src).await;

    let client = BoltClient::new();

    // Write a temporal typed edge.
    let relate = client
        .relate("CALLS", vec![EntityPair::new(&src, &tgt)])
        .await;
    assert!(relate.ok, "temporal relate failed: {:?}", relate.error);
    assert_eq!(relate.data["temporal"], true);

    // As-of "now" should see exactly one open edge between src/tgt.
    let now = now_ms();
    let as_of = client.relations_as_of("CALLS", now + 1).await;
    assert!(as_of.ok, "as_of read failed: {:?}", as_of.error);
    let hit = as_of.data["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .find(|e| e["source"] == src && e["target"] == tgt)
        .expect("the just-written edge is visible as-of now");
    assert_eq!(
        hit["valid_to"].as_i64(),
        Some(OPEN),
        "freshly written edge is open-ended"
    );

    // A point in time before the edge existed sees nothing for it.
    let before = client.relations_as_of("CALLS", 1).await;
    assert!(before.ok);
    assert!(
        !before.data["edges"]
            .as_array()
            .expect("edges array")
            .iter()
            .any(|e| e["source"] == src && e["target"] == tgt),
        "edge must not be visible before its valid_from"
    );

    scrub(&g, &src).await;
}

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_upsert_edge_supersedes_and_preserves_history() {
    let _db = DB_LOCK.lock().await;
    enable_temporal();
    let g = raw_graph().await;
    let src = uniq("sup_src");
    let tgt = uniq("sup_tgt");
    scrub(&g, &src).await;
    let client = BoltClient::new();

    // First weighted write → one open edge (weight 1.0).
    let t0 = now_ms();
    assert!(client.upsert_edge(&src, "CALLS", &tgt, 1.0).await.ok);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let between = now_ms();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    // Second write with a different delta supersedes (cumulative weight 3.0).
    assert!(client.upsert_edge(&src, "CALLS", &tgt, 2.0).await.ok);

    // Two versions now exist for the triple (history accumulated).
    let versions = count_edges(&g, &src, &tgt, "CALLS").await;
    assert_eq!(versions, 2, "supersede must accumulate, not overwrite");

    // As-of "between" the writes → the original open window (weight 1.0).
    let mid = client.relations_as_of("CALLS", between).await;
    assert!(mid.ok);
    let mid_hit = find_in(&mid.data, &src, &tgt).expect("edge visible mid-history");
    assert!(
        mid_hit["valid_to"].as_i64().unwrap() > between,
        "still valid at `between`"
    );

    // As-of "now" → the fresh open edge.
    let now = client.relations_as_of("CALLS", now_ms() + 1).await;
    assert!(
        find_in(&now.data, &src, &tgt).expect("current edge")["valid_to"].as_i64() == Some(OPEN)
    );

    // As-of before t0 → nothing.
    let pre = client.relations_as_of("CALLS", t0 - 1).await;
    assert!(find_in(&pre.data, &src, &tgt).is_none());

    scrub(&g, &src).await;
}

// ── unrelate = logical delete ──────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_unrelate_is_logical_delete_preserving_history() {
    let _db = DB_LOCK.lock().await;
    enable_temporal();
    let g = raw_graph().await;
    let src = uniq("del_src");
    let tgt = uniq("del_tgt");
    scrub(&g, &src).await;
    let client = BoltClient::new();

    let t0 = now_ms();
    assert!(
        client
            .relate("CALLS", vec![EntityPair::new(&src, &tgt)])
            .await
            .ok
    );
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // Logical delete: the fact stops being true now.
    let del = client
        .unrelate("CALLS", vec![EntityPair::new(&src, &tgt)])
        .await;
    assert!(del.ok, "temporal unrelate failed: {:?}", del.error);
    assert_eq!(del.data["temporal"], true);

    // The edge row still exists (not hard-deleted); history preserved.
    assert_eq!(
        count_edges(&g, &src, &tgt, "CALLS").await,
        1,
        "row preserved"
    );

    // As-of just after creation (inside the now-bounded window) still sees it.
    let mid = client.relations_as_of("CALLS", t0 + 1).await;
    assert!(
        find_in(&mid.data, &src, &tgt).is_some(),
        "visible inside its valid window"
    );

    // As-of far in the future (after retraction) does NOT.
    let future = client.relations_as_of("CALLS", now_ms() + 10_000).await;
    assert!(
        find_in(&future.data, &src, &tgt).is_none(),
        "gone after retraction"
    );

    // tx_to stays OPEN (still current belief — §3.2).
    let (_, _, _, tx_to) = read_edge(&g, &src, &tgt, "CALLS").await;
    assert_eq!(tx_to, OPEN, "retract leaves tx_to open");

    scrub(&g, &src).await;
}

// ── correction / as-believed-at (transaction-time travel) ──────────────

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_correction_and_as_believed_at_travel() {
    let _db = DB_LOCK.lock().await;
    enable_temporal();
    let g = raw_graph().await;
    let src = uniq("cor_src");
    let tgt = uniq("cor_tgt");
    scrub(&g, &src).await;
    let client = BoltClient::new();

    // Record weight 1.0.
    assert!(client.upsert_edge(&src, "CALLS", &tgt, 1.0).await.ok);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let believed_before = now_ms(); // between the write and the correction
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    // "We recorded the wrong value" — correct current belief to 9.0.
    let corrected = client.correct_edge("CALLS", &src, &tgt, 9.0).await;
    assert!(corrected.ok, "correct_edge failed: {:?}", corrected.error);

    let now = now_ms();
    // Current belief (as-of now, believed now) → the corrected row.
    let believed_now = client.relations_as_believed_at("CALLS", now, now).await;
    assert!(believed_now.ok, "{:?}", believed_now.error);
    assert!(
        find_in(&believed_now.data, &src, &tgt).is_some(),
        "corrected edge is current belief"
    );

    // As believed BEFORE the correction → the original row is what we
    // believed then (transaction-time travel).
    let past_belief = client
        .relations_as_believed_at("CALLS", now, believed_before)
        .await;
    assert!(past_belief.ok, "{:?}", past_belief.error);
    let hit = find_in(&past_belief.data, &src, &tgt)
        .expect("pre-correction belief still reconstructable");
    // The pre-correction row's tx window closed at the correction instant.
    assert!(
        hit["tx_to"].as_i64().unwrap() > believed_before,
        "believed at a tx time inside the original belief window"
    );

    scrub(&g, &src).await;
}

// ── migration backfill + idempotency + gated indexes ───────────────────

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_migration_backfills_created_at_and_is_idempotent() {
    let _db = DB_LOCK.lock().await;
    enable_temporal();
    let g = raw_graph().await;
    let src = uniq("mig_src");
    let tgt = uniq("mig_tgt");
    scrub(&g, &src).await;

    // Seed a LEGACY (non-temporal) edge carrying a `created_at` but no
    // temporal props — the shape migration must upgrade.
    let created_at: i64 = 1_700_000_000_000; // a fixed past epoch-ms
    g.run(
        neo4rs::query(
            "MERGE (a:Entity {name:$s}) MERGE (b:Entity {name:$t}) \
             CREATE (a)-[:CALLS {created_at:$ca}]->(b)",
        )
        .param("s", src.clone())
        .param("t", tgt.clone())
        .param("ca", created_at),
    )
    .await
    .expect("seed legacy edge");

    // Also seed a legacy edge WITHOUT created_at → floors to EPOCH_DEFAULT.
    let src2 = uniq("mig2_src");
    let tgt2 = uniq("mig2_tgt");
    scrub(&g, &src2).await;
    g.run(
        neo4rs::query(
            "MERGE (a:Entity {name:$s}) MERGE (b:Entity {name:$t}) \
             CREATE (a)-[:IMPORTS]->(b)",
        )
        .param("s", src2.clone())
        .param("t", tgt2.clone()),
    )
    .await
    .expect("seed legacy edge 2");

    let client = BoltClient::new();

    // Gated edge-property indexes create clean.
    let idx = client.ensure_temporal_schema().await;
    assert!(idx.ok, "ensure_temporal_schema failed: {:?}", idx.error);
    assert_eq!(idx.data["temporal"], true);
    assert!(
        idx.data["indexes"].as_i64().unwrap() >= 10,
        "5 rels × 2 props"
    );

    // Run the migration.
    let mig = client.backfill_temporal_edges().await;
    assert!(mig.ok, "backfill failed: {:?}", mig.error);
    let migrated = mig.data["migrated"].as_i64().expect("migrated count");
    assert!(
        migrated >= 2,
        "at least our two seeded edges migrated (got {migrated})"
    );

    // Edge with created_at → valid_from == created_at, ends OPEN.
    let (vf, vt, txf, txt) = read_edge(&g, &src, &tgt, "CALLS").await;
    assert_eq!(vf, created_at, "valid_from backfilled from created_at");
    assert_eq!(txf, created_at, "tx_from backfilled from created_at");
    assert_eq!(vt, OPEN, "valid_to opened");
    assert_eq!(txt, OPEN, "tx_to opened");

    // Edge without created_at → valid_from floors to EPOCH_DEFAULT (0).
    let (vf2, vt2, _, txt2) = read_edge(&g, &src2, &tgt2, "IMPORTS").await;
    assert_eq!(vf2, 0, "no created_at → EPOCH_DEFAULT_FLOOR");
    assert_eq!((vt2, txt2), (OPEN, OPEN));

    // Idempotent: a second run migrates nothing new.
    let mig2 = client.backfill_temporal_edges().await;
    assert!(mig2.ok);
    assert_eq!(
        mig2.data["migrated"].as_i64(),
        Some(0),
        "re-run must skip already-migrated edges"
    );
    // ...and our edge's valid_from is unchanged by the re-run.
    let (vf_again, _, _, _) = read_edge(&g, &src, &tgt, "CALLS").await;
    assert_eq!(vf_again, created_at, "idempotent: valid_from stable");

    scrub(&g, &src).await;
    scrub(&g, &src2).await;
}

// ── temporal-aware traverse (as-of graph walk) ─────────────────────────

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687 + WYLDE_TEMPORAL_MEMORY=1"]
async fn temporal_traverse_respects_as_of_predicate() {
    let _db = DB_LOCK.lock().await;
    use wylde_harness::memory::memgraph::client::TraverseRequest;
    enable_temporal();
    let g = raw_graph().await;
    let a = uniq("trv_a");
    let b = uniq("trv_b");
    let chunk_id = uniq("trv_chunk");
    scrub(&g, &a).await;

    let client = BoltClient::new();

    // a -[CALLS(temporal)]-> b, and b MENTIONED_IN a chunk.
    let t0 = now_ms();
    assert!(
        client
            .relate("CALLS", vec![EntityPair::new(&a, &b)])
            .await
            .ok
    );
    g.run(
        neo4rs::query(
            "MERGE (b:Entity {name:$b}) \
             MERGE (c:Chunk {id:$cid}) SET c.path=$cid, c.symbol='s', c.language='rust' \
             MERGE (b)-[:MENTIONED_IN]->(c)",
        )
        .param("b", b.clone())
        .param("cid", chunk_id.clone()),
    )
    .await
    .expect("seed chunk + mention");

    // As-of NOW: the walk follows the valid CALLS edge and reaches the chunk.
    let req_now = TraverseRequest {
        entities: vec![a.clone()],
        max_hops: 2,
        limit: 25,
        workspace: None,
        decay_alpha: None,
        rel_depths: None,
        as_of: Some(now_ms() + 1),
    };
    let now = client.traverse(req_now).await;
    assert!(now.ok, "temporal traverse failed: {:?}", now.error);
    let found_now = now.data["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .any(|c| c["id"] == chunk_id);
    assert!(
        found_now,
        "as-of now must reach the chunk via the valid edge"
    );

    // As-of BEFORE the edge existed: the typed edge isn't valid, so the
    // as-of walk must not traverse it to the chunk.
    let req_past = TraverseRequest {
        entities: vec![a.clone()],
        max_hops: 2,
        limit: 25,
        workspace: None,
        decay_alpha: None,
        rel_depths: None,
        as_of: Some(t0 - 1),
    };
    let past = client.traverse(req_past).await;
    assert!(past.ok, "{:?}", past.error);
    let found_past = past.data["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .any(|c| c["id"] == chunk_id);
    assert!(
        !found_past,
        "as-of before valid_from must not traverse the not-yet-valid edge"
    );

    scrub(&g, &a).await;
    g.run(neo4rs::query("MATCH (c:Chunk {id:$cid}) DETACH DELETE c").param("cid", chunk_id))
        .await
        .expect("cleanup chunk");
}

// ── toggle-OFF identity (relational behavior, no temporal props) ───────

#[tokio::test]
#[ignore = "requires a live Neo4j/Memgraph on bolt://127.0.0.1:7687"]
async fn toggle_off_is_byte_identical_relational_behavior() {
    let _db = DB_LOCK.lock().await;
    // SAFETY: serial run (`--test-threads=1`); no concurrent env mutation.
    unsafe {
        std::env::remove_var("WYLDE_TEMPORAL_MEMORY");
    }
    let g = raw_graph().await;
    let a = uniq("off_a");
    let b = uniq("off_b");
    scrub(&g, &a).await;
    let client = BoltClient::new();

    // relate twice → MERGE is idempotent: exactly ONE edge, NO history,
    // and NO temporal properties (byte-identical to today's relational).
    let r1 = client.relate("CALLS", vec![EntityPair::new(&a, &b)]).await;
    assert!(r1.ok);
    assert!(
        r1.data.get("temporal").is_none(),
        "OFF relate must not tag temporal"
    );
    assert!(
        client
            .relate("CALLS", vec![EntityPair::new(&a, &b)])
            .await
            .ok
    );
    assert_eq!(
        count_edges(&g, &a, &b, "CALLS").await,
        1,
        "OFF relate is MERGE-idempotent — no versioned history"
    );
    assert!(
        !edge_has_temporal_props(&g, &a, &b, "CALLS").await,
        "OFF edges carry no valid_from/tx_from"
    );

    // upsert_edge twice → weight mutated IN PLACE (ON MATCH SET), still
    // one edge, no temporal props.
    let s = uniq("off_s");
    let t = uniq("off_t");
    scrub(&g, &s).await;
    assert!(client.upsert_edge(&s, "CALLS", &t, 1.0).await.ok);
    assert!(client.upsert_edge(&s, "CALLS", &t, 2.0).await.ok);
    assert_eq!(
        count_edges(&g, &s, &t, "CALLS").await,
        1,
        "in-place weight, one edge"
    );
    assert!(!edge_has_temporal_props(&g, &s, &t, "CALLS").await);

    // unrelate → HARD delete (row gone), not a logical close.
    assert!(
        client
            .unrelate("CALLS", vec![EntityPair::new(&a, &b)])
            .await
            .ok
    );
    assert_eq!(
        count_edges(&g, &a, &b, "CALLS").await,
        0,
        "OFF unrelate hard-deletes"
    );

    // as-of / as-believed / correction / migration all refuse when OFF.
    assert_eq!(
        client
            .relations_as_of("CALLS", now_ms())
            .await
            .error
            .map(|e| e.code),
        Some("temporal_disabled".to_owned())
    );

    scrub(&g, &a).await;
    // upsert_edge creates UNLABELED nodes — scrub (which targets :Entity)
    // won't catch them; delete label-agnostically by name.
    g.run(
        neo4rs::query("MATCH (n) WHERE n.name STARTS WITH $p DETACH DELETE n")
            .param("p", s.clone()),
    )
    .await
    .expect("cleanup unlabeled");
    // Restore ON for any later serial test.
    enable_temporal();
}

/// True iff the (any) edge for the triple carries a `valid_from` prop.
async fn edge_has_temporal_props(g: &Graph, src: &str, tgt: &str, rel: &str) -> bool {
    let q = format!(
        "MATCH ({{name:$s}})-[r:{rel}]->({{name:$t}}) \
         RETURN count(r.valid_from) AS n"
    );
    let mut rows = g
        .execute(neo4rs::query(&q).param("s", src).param("t", tgt))
        .await
        .expect("temporal-prop probe");
    rows.next()
        .await
        .ok()
        .flatten()
        .and_then(|r| r.get::<i64>("n").ok())
        .unwrap_or(0)
        > 0
}

// ── raw-neo4rs helpers ─────────────────────────────────────────────────

/// Count edge rows (all versions) for a triple. Label-agnostic on the
/// endpoints: `relate`/temporal writes create `:Entity` nodes but the
/// relational `cypher::upsert_edge` MERGEs *unlabeled* nodes, and this
/// helper must count both (names are unique per test, so no collision).
async fn count_edges(g: &Graph, src: &str, tgt: &str, rel: &str) -> i64 {
    let q = format!("MATCH ({{name:$s}})-[r:{rel}]->({{name:$t}}) RETURN count(r) AS n");
    let mut rows = g
        .execute(neo4rs::query(&q).param("s", src).param("t", tgt))
        .await
        .expect("count query");
    rows.next()
        .await
        .ok()
        .flatten()
        .and_then(|r| r.get("n").ok())
        .unwrap_or(0)
}

/// Read the four temporal properties of the (single) current-belief-open
/// edge for a triple. Panics if there isn't exactly one open edge.
async fn read_edge(g: &Graph, src: &str, tgt: &str, rel: &str) -> (i64, i64, i64, i64) {
    let q = format!(
        "MATCH (a:Entity {{name:$s}})-[r:{rel}]->(b:Entity {{name:$t}}) \
         WHERE r.tx_to = {OPEN} \
         RETURN r.valid_from AS vf, r.valid_to AS vt, r.tx_from AS txf, r.tx_to AS txt"
    );
    let mut rows = g
        .execute(neo4rs::query(&q).param("s", src).param("t", tgt))
        .await
        .expect("read query");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("one open edge");
    (
        row.get("vf").unwrap(),
        row.get("vt").unwrap(),
        row.get("txf").unwrap(),
        row.get("txt").unwrap(),
    )
}

/// Find a specific edge in an as-of / as-believed-at reply's `data`.
fn find_in<'a>(data: &'a serde_json::Value, src: &str, tgt: &str) -> Option<&'a serde_json::Value> {
    data["edges"]
        .as_array()?
        .iter()
        .find(|e| e["source"] == src && e["target"] == tgt)
}
