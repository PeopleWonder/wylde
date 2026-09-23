//! Live smoke test for the direct-Bolt path in
//! `crate::memory::memgraph::bolt`.
//!
//! Drives [`wylde_harness::memory::memgraph::BoltClient`] against a
//! real Neo4j over `bolt://127.0.0.1:7687` (whatever `GRAPH_BOLT_URL`
//! points at). On the Wylde user's machine that's the bundled Neo4j JVM the
//! Python `Core/Memgraph/run.py` supervises.
//!
//! Why `#[ignore]`: depends on Neo4j being live at the Bolt port and
//! the Wylde user's `auth=None` config. CI doesn't have that, so the tests
//! stay off by default. Run with:
//!
//! ```bash
//! cargo test -p wylde-harness --test memgraph_bolt_integration -- --ignored --nocapture
//! ```
//!
//! Sister test `memgraph_integration.rs` covers the IPC-via-Python-
//! service path (the `python` strangler-fig branch); this one covers
//! the direct-Bolt path (the `rust` branch). Keeping them in
//! separate files makes it explicit which test exercises which
//! transport when a regression shows up.
//!
//! Each test uses a randomised workspace label so writes never touch
//! production graph state. Cleanup runs via `delete_workspace` at the
//! end of each test (and as the first step of each test in case a
//! prior aborted run left rows behind).

use serde_json::json;
use wylde_harness::memory::memgraph::{BoltClient, EntityPair, TraverseRequest};

/// Serialize every test in this binary against the one shared Neo4j (#83).
///
/// `cargo test` runs a binary's tests multi-threaded by default. Per-workspace
/// data is nonce-namespaced by [`test_workspace`], but the `Entity` table is
/// **global-by-name** (these tests seed bare labels like `shared_entity` /
/// `alpha_seed`), and `stats()` / `ensure_schema` / the orphan-entity prune in
/// `delete_workspace` are graph-wide. Run concurrently against the shared DB
/// those contend — the exact self-collision `#216` fixed for the sister
/// `wylde-workspaces` `integration_graph` binary and `memgraph_live`. CI passes
/// `--test-threads=1` as a uniform guard, but a developer's ad-hoc `--ignored`
/// run does not; holding this lock for each test body makes the serialization a
/// property of the test, not of how it happens to be invoked. Distinct
/// workspace/entity prefixes remain layered on top as namespace hygiene.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Generate a workspace label that won't collide with real data —
/// includes both a static prefix (so `delete_workspace` against the
/// prefix wouldn't be needed in cleanup; the label is unique anyway)
/// and a per-process nonce.
fn test_workspace() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nonce = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "wylde-rust-bolt-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
        nonce
    )
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687 (e.g. via wylde-memgraph service)"]
async fn health_returns_ok_against_live_neo4j() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let cfg = client.config().clone();
    eprintln!("connecting to {} (user={:?})", cfg.uri, cfg.user);
    let reply = client.health().await;
    if !reply.ok {
        eprintln!("health failed: {:?}", reply.error);
    }
    assert!(reply.ok, "live health probe must return ok=true");
    let data = reply.data;
    assert_eq!(
        data["ok"], true,
        "health payload should be {{\"ok\": true}}, got {data:?}"
    );
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn ensure_schema_returns_ok() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let reply = client.ensure_schema().await;
    assert!(
        reply.ok,
        "ensure_schema must return ok; got {:?}",
        reply.error
    );
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn upsert_then_traverse_round_trip_returns_seeded_chunks() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let ws = test_workspace();
    eprintln!("using workspace {ws}");

    // Belt-and-braces cleanup of any stale rows under this label (the
    // nonce makes that extremely unlikely, but cheap to guarantee).
    let _ = client.delete_workspace(&ws).await;

    let chunks = vec![
        json!({
            "id": format!("{ws}-c1"),
            "path": "alpha.py",
            "symbol": "alpha",
            "language": "python",
            "workspace": ws,
            "entities": ["alpha_seed", "shared_entity"],
        }),
        json!({
            "id": format!("{ws}-c2"),
            "path": "beta.py",
            "symbol": "beta",
            "language": "python",
            "workspace": ws,
            "entities": ["beta_only", "shared_entity"],
        }),
    ];
    let up = client.upsert(chunks).await;
    assert!(up.ok, "upsert must succeed; got {:?}", up.error);
    assert_eq!(up.data["count"], 2);

    // Traverse from the shared entity — both chunks should come back,
    // even though only one of them lists alpha_seed.
    let req = TraverseRequest {
        entities: vec!["shared_entity".into()],
        max_hops: 1,
        limit: 10,
        workspace: Some(ws.clone()),
        decay_alpha: None,
        rel_depths: None,
    };
    let trv = client.traverse(req).await;
    assert!(trv.ok, "traverse must succeed; got {:?}", trv.error);
    let returned: Vec<&str> = trv.data["chunks"]
        .as_array()
        .expect("chunks array")
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
        .collect();
    assert!(
        returned.iter().any(|id| id.ends_with("-c1")),
        "c1 should be returned: {returned:?}"
    );
    assert!(
        returned.iter().any(|id| id.ends_with("-c2")),
        "c2 should be returned: {returned:?}"
    );

    // Cleanup — delete the workspace's chunks. We assert ok but don't
    // require count==2 (the orphan prune count varies if other tests
    // happen to be running in parallel against the same DB).
    let del = client.delete_workspace(&ws).await;
    assert!(del.ok, "delete_workspace must succeed; got {:?}", del.error);
    assert_eq!(del.data["chunks_deleted"], 2);
}

/// Regression test for the **Python `traverse` workspace-drop bug**.
///
/// The Python `Core/harness/memory/memgraph.py::traverse` signature
/// didn't accept `workspace`, so when callers tried to pass it, the
/// `TypeError` fallback in `_try_traverse` dropped the filter and the
/// entire DB was searched. This Rust port forwards `workspace`
/// through to the route — verify it actually filters.
#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn traverse_workspace_filter_excludes_other_workspaces() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let ws_keep = test_workspace();
    let ws_other = test_workspace();
    let _ = client.delete_workspace(&ws_keep).await;
    let _ = client.delete_workspace(&ws_other).await;

    let chunks = vec![
        json!({
            "id": format!("{ws_keep}-keep"),
            "path": "keep.py",
            "workspace": ws_keep,
            "entities": ["pinned_entity"],
        }),
        json!({
            "id": format!("{ws_other}-skip"),
            "path": "skip.py",
            "workspace": ws_other,
            "entities": ["pinned_entity"],
        }),
    ];
    let up = client.upsert(chunks).await;
    assert!(up.ok);

    // Traverse filtered to ws_keep — only "keep.py" should come back.
    let trv = client
        .traverse(TraverseRequest {
            entities: vec!["pinned_entity".into()],
            max_hops: 1,
            limit: 10,
            workspace: Some(ws_keep.clone()),
            decay_alpha: None,
            rel_depths: None,
        })
        .await;
    assert!(trv.ok);
    let ids: Vec<String> = trv.data["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    assert!(
        ids.iter().any(|id| id.ends_with("-keep")),
        "keep chunk should be returned: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.ends_with("-skip")),
        "skip chunk MUST be filtered out by workspace: {ids:?}"
    );

    // Cleanup.
    let _ = client.delete_workspace(&ws_keep).await;
    let _ = client.delete_workspace(&ws_other).await;
}

/// Regression test for the **Python `multihop` field-name bug**.
///
/// The Python `Core/harness/memory/memgraph.py::multihop` sent
/// `{"start": [...], "max_hops": N}` but the server route reads
/// `{"entities": [...], "expand_hops": N}`. Every Python multihop
/// call arrived at the server with an empty entity list and returned
/// `chunks: []` silently. The Rust port uses the right parameter
/// names by construction (it's a direct Cypher binding now), so the
/// bug is structurally impossible. Verify by writing data, then
/// asking multihop for it.
#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn multihop_returns_chunks_for_known_entities() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let ws = test_workspace();
    let _ = client.delete_workspace(&ws).await;

    // Two chunks sharing a seed entity — the bipartite traversal
    // should walk seed→chunk→other_entity→chunk and surface both.
    let chunks = vec![
        json!({
            "id": format!("{ws}-mh1"),
            "path": "mh1.py",
            "workspace": ws,
            "entities": ["mh_seed", "mh_extra_a"],
        }),
        json!({
            "id": format!("{ws}-mh2"),
            "path": "mh2.py",
            "workspace": ws,
            "entities": ["mh_seed", "mh_extra_b"],
        }),
    ];
    let up = client.upsert(chunks).await;
    assert!(up.ok, "upsert failed: {:?}", up.error);

    let mh = client.multihop(vec!["mh_seed".into()], 1, 10).await;
    assert!(mh.ok, "multihop failed: {:?}", mh.error);
    // The expanded set must include the seed (so chunks step 2
    // returns rows). Python's bug meant the seed was never sent and
    // expanded was always [].
    let expanded: Vec<&str> = mh.data["expanded_entities"]
        .as_array()
        .expect("expanded array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        expanded.contains(&"mh_seed"),
        "expanded must contain seed; got {expanded:?}"
    );
    // And chunks step must come back non-empty for valid input.
    let ids: Vec<&str> = mh.data["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
        .collect();
    assert!(
        ids.iter()
            .any(|id| id.ends_with("-mh1") || id.ends_with("-mh2")),
        "multihop must return seeded chunks; got {ids:?}"
    );

    let _ = client.delete_workspace(&ws).await;
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn relate_unrelate_round_trip() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    // Use a unique prefix so we don't disturb existing Entity nodes
    // (Entity isn't workspace-scoped on the upsert side).
    let prefix = format!("rust_bolt_rel_{}", std::process::id());
    let a = format!("{prefix}_a");
    let b = format!("{prefix}_b");
    let c = format!("{prefix}_c");

    // Create the edges.
    let pairs = vec![EntityPair::new(&a, &b), EntityPair::new(&a, &c)];
    let r = client.relate("CALLS", pairs).await;
    assert!(r.ok, "relate failed: {:?}", r.error);
    assert_eq!(r.data["written"], 2);

    // Drop them.
    let pairs = vec![EntityPair::new(&a, &b), EntityPair::new(&a, &c)];
    let u = client.unrelate("CALLS", pairs).await;
    assert!(u.ok, "unrelate failed: {:?}", u.error);
    assert_eq!(u.data["deleted"], 2);
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn relate_rejects_unknown_rel_type() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let r = client
        .relate("MADE_UP", vec![EntityPair::new("x", "y")])
        .await;
    assert!(!r.ok, "relate must reject unknown rel_type");
    assert_eq!(r.error.expect("err envelope").code, "bad_request");
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn upsert_edge_succeeds_on_valid_label() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let prefix = format!("rust_bolt_edge_{}", std::process::id());
    let r = client
        .upsert_edge(
            &format!("{prefix}_src"),
            "MENTIONS",
            &format!("{prefix}_tgt"),
            1.5,
        )
        .await;
    assert!(r.ok, "upsert_edge failed: {:?}", r.error);
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn upsert_edge_rejects_invalid_label() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    // Spaces and punctuation aren't valid Cypher rel types.
    let r = client.upsert_edge("src", "BAD LABEL!", "tgt", 1.0).await;
    assert!(!r.ok, "upsert_edge must reject invalid label");
    assert_eq!(r.error.expect("err envelope").code, "bad_request");
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn stats_returns_all_five_counts() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let s = client.stats().await;
    assert!(s.ok, "stats failed: {:?}", s.error);
    for key in [
        "entities",
        "chunks",
        "mentions",
        "communities",
        "typed_relationships",
    ] {
        assert!(
            s.data.get(key).and_then(|v| v.as_i64()).is_some(),
            "stats must surface {key} as an integer; got {:?}",
            s.data
        );
    }
}

#[tokio::test]
#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]
async fn delete_workspace_returns_chunk_count() {
    let _db = DB_LOCK.lock().await;
    let client = BoltClient::new();
    let ws = test_workspace();
    let _ = client.delete_workspace(&ws).await; // pre-clean

    let chunks = vec![
        json!({"id": format!("{ws}-a"), "workspace": ws, "entities": ["e"]}),
        json!({"id": format!("{ws}-b"), "workspace": ws, "entities": ["e"]}),
        json!({"id": format!("{ws}-c"), "workspace": ws, "entities": ["e"]}),
    ];
    let _ = client.upsert(chunks).await;

    let del = client.delete_workspace(&ws).await;
    assert!(del.ok);
    assert_eq!(del.data["chunks_deleted"], 3);
    // orphan_entities_deleted >= 0; we don't pin a specific value
    // because parallel tests may create / delete `e` concurrently.
    assert!(del.data["orphan_entities_deleted"].as_i64().is_some());
}
