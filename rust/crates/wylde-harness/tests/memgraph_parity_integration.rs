//! Cross-impl parity gate for the memgraph strangler-fig.
//!
//! Exercises each of the 11 verbs through BOTH transports against the
//! same live Neo4j:
//!
//! * `Pipe`  = [`wylde_harness::memory::memgraph::Client`]
//!   (msgpack-over-named-pipe → Python `Core/Memgraph/`
//!   Flask → Neo4j)
//! * `Bolt`  = [`wylde_harness::memory::memgraph::BoltClient`]
//!   (direct `neo4rs` → Neo4j)
//!
//! If both transports produce envelope-compatible replies for every
//! verb, the strangler-fig switch (`WYLDE_HARNESS_MEMORY_IMPL`) can be
//! flipped from `python` → `rust` with no behavioural regression. If
//! any verb diverges, this test names it precisely so a follow-up
//! slice can fix the divergence before the flip lands.
//!
//! ## Why `#[ignore]`
//!
//! Both transports require live infrastructure:
//!
//! * Bolt path → Neo4j on `bolt://127.0.0.1:7687`
//! * Pipe path → the `wylde-memgraph` Python service on
//!   `\\.\pipe\wylde-memgraph` (which in turn supervises Neo4j)
//!
//! CI doesn't have either. The unit tests under
//! `src/memory/memgraph/` cover every code path against mocks; this
//! file is the live-stack gate run on demand:
//!
//! ```bash
//! cargo test -p wylde-harness --test memgraph_parity_integration \
//!     -- --ignored --nocapture
//! ```
//!
//! ## Test isolation
//!
//! Every test uses a fresh nonce-stamped workspace so parallel runs
//! and prior aborted runs cannot contaminate. Each test cleans up via
//! `delete_workspace` at start AND end. Tests also use a unique
//! `prefix` for Entity names that aren't workspace-scoped, so cross-
//! test interference on the shared `Entity` table is minimised.
//!
//! ## Per-verb result interpretation
//!
//! For verbs whose data is deterministic (counts, ok-only envelopes)
//! the test asserts exact equality across transports. For verbs whose
//! payload contains floating-point scores or ranked lists, the test
//! asserts structural parity (key presence + ID-set equality) — the
//! precise score values can differ in low-order bits between Python's
//! `math.exp` and Rust's `f64::exp` and that's not a real divergence.

use serde_json::{json, Value};
use std::time::Duration;

use wylde_harness::memory::memgraph::client::{Client, TraverseRequest as PipeTraverseRequest};
use wylde_harness::memory::memgraph::{BoltClient, EntityPair, TraverseRequest};
use wylde_shared::ipc::Reply;

/// Per-test nonce → unique workspace label that no production data
/// can collide with. Same shape as the sister `memgraph_bolt_integration`
/// test's `test_workspace()` helper.
fn test_workspace(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nonce = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "wylde-parity-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
        nonce
    )
}

/// Build a Pipe client wired to the running service. Honours
/// `WYLDE_MEMGRAPH_SERVICE` like the production client.
fn pipe_client() -> Client {
    // The shared IPC respects `WYLDE_IPC_DISABLE` — make sure a sibling
    // test's leftover doesn't gag us mid-run.
    // SAFETY: single-threaded test setup.
    unsafe {
        std::env::remove_var("WYLDE_IPC_DISABLE");
    }
    Client::new().with_timeout(Duration::from_secs(10))
}

/// Build a Bolt client wired to the running Neo4j via env defaults.
fn bolt_client() -> BoltClient {
    BoltClient::new()
}

/// Tag a verb's reply with which transport produced it, for the
/// per-verb pass/fail eprintln stream that becomes the report.
fn log_pair(verb: &str, pipe: &Reply, bolt: &Reply) {
    eprintln!(
        "parity[{verb}]: pipe ok={} err={:?} ; bolt ok={} err={:?}",
        pipe.ok, pipe.error, bolt.ok, bolt.error
    );
    eprintln!("  pipe.data = {}", pipe.data);
    eprintln!("  bolt.data = {}", bolt.data);
}

/// Helper: extract `data["ok"]` as a bool (the inner ok the route
/// returns, distinct from the IPC envelope `Reply::ok`).
fn inner_ok(reply: &Reply) -> Option<bool> {
    reply.data.get("ok").and_then(Value::as_bool)
}

// ── 1. health ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_01_health() {
    let pipe = pipe_client().health().await;
    let bolt = bolt_client().health().await;
    log_pair("health", &pipe, &bolt);

    assert!(pipe.ok, "pipe health must return ok=true");
    assert!(bolt.ok, "bolt health must return ok=true");
    assert_eq!(
        inner_ok(&pipe),
        Some(true),
        "pipe data.ok must be true"
    );
    assert_eq!(
        inner_ok(&bolt),
        Some(true),
        "bolt data.ok must be true"
    );
}

// ── 2. ensure_schema ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_02_ensure_schema() {
    let pipe = pipe_client().ensure_schema().await;
    let bolt = bolt_client().ensure_schema().await;
    log_pair("ensure_schema", &pipe, &bolt);

    assert!(pipe.ok, "pipe ensure_schema must return ok=true");
    assert!(bolt.ok, "bolt ensure_schema must return ok=true");
    assert_eq!(inner_ok(&pipe), Some(true));
    assert_eq!(inner_ok(&bolt), Some(true));
}

// ── 3. upsert ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_03_upsert() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let ws = test_workspace("upsert");

    // pre-clean
    let _ = bolt_c.delete_workspace(&ws).await;

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

    let pipe = pipe_c.upsert(chunks.clone()).await;
    let bolt = bolt_c.upsert(chunks).await;
    log_pair("upsert", &pipe, &bolt);

    let _ = bolt_c.delete_workspace(&ws).await;

    assert!(pipe.ok && bolt.ok, "both must succeed");
    assert_eq!(
        pipe.data["count"], bolt.data["count"],
        "upsert count must match across transports"
    );
    assert_eq!(pipe.data["count"], 2, "expected 2 rows");
}

// ── 4. delete_path ────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_04_delete_path() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let ws = test_workspace("dpath");
    let _ = bolt_c.delete_workspace(&ws).await;

    // Seed via bolt so both deletes have something to operate against.
    let path_a = format!("parity-{ws}-A.py");
    let path_b = format!("parity-{ws}-B.py");
    let _ = bolt_c
        .upsert(vec![
            json!({"id": format!("{ws}-a"), "path": path_a, "workspace": ws, "entities": ["x"]}),
            json!({"id": format!("{ws}-b"), "path": path_b, "workspace": ws, "entities": ["y"]}),
        ])
        .await;

    let pipe = pipe_c.delete_path(&path_a).await;
    let bolt = bolt_c.delete_path(&path_b).await;
    log_pair("delete_path", &pipe, &bolt);
    let _ = bolt_c.delete_workspace(&ws).await;

    assert!(pipe.ok && bolt.ok, "both delete_path must succeed");
    assert_eq!(inner_ok(&pipe), Some(true));
    assert_eq!(inner_ok(&bolt), Some(true));
}

// ── 5. delete_workspace ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_05_delete_workspace() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let ws_pipe = test_workspace("dwspipe");
    let ws_bolt = test_workspace("dwsbolt");
    let _ = bolt_c.delete_workspace(&ws_pipe).await;
    let _ = bolt_c.delete_workspace(&ws_bolt).await;

    let seed = |ws: &str| {
        let ws = ws.to_owned();
        vec![
            json!({"id": format!("{ws}-a"), "workspace": ws, "entities": ["e1"]}),
            json!({"id": format!("{ws}-b"), "workspace": ws, "entities": ["e1"]}),
            json!({"id": format!("{ws}-c"), "workspace": ws, "entities": ["e1"]}),
        ]
    };
    let _ = bolt_c.upsert(seed(&ws_pipe)).await;
    let _ = bolt_c.upsert(seed(&ws_bolt)).await;

    let pipe = pipe_c.delete_workspace(&ws_pipe).await;
    let bolt = bolt_c.delete_workspace(&ws_bolt).await;
    log_pair("delete_workspace", &pipe, &bolt);

    assert!(pipe.ok && bolt.ok);
    assert_eq!(
        pipe.data["chunks_deleted"], bolt.data["chunks_deleted"],
        "chunks_deleted must match"
    );
    assert_eq!(pipe.data["chunks_deleted"], 3, "expected 3 chunks");
    // workspace echoed back identically (modulo the input we sent each
    // transport — same shape, different value).
    assert_eq!(pipe.data["workspace"], ws_pipe);
    assert_eq!(bolt.data["workspace"], ws_bolt);
}

// ── 6. traverse ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_06_traverse() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let ws = test_workspace("trv");
    let _ = bolt_c.delete_workspace(&ws).await;

    let _ = bolt_c
        .upsert(vec![
            json!({
                "id": format!("{ws}-c1"), "path": "a.py", "workspace": ws,
                "entities": ["shared_entity"],
            }),
            json!({
                "id": format!("{ws}-c2"), "path": "b.py", "workspace": ws,
                "entities": ["shared_entity"],
            }),
        ])
        .await;

    let pipe_req = PipeTraverseRequest {
        entities: vec!["shared_entity".into()],
        max_hops: 1,
        limit: 10,
        workspace: Some(ws.clone()),
        decay_alpha: None,
        rel_depths: None,
    };
    let bolt_req = TraverseRequest {
        entities: vec!["shared_entity".into()],
        max_hops: 1,
        limit: 10,
        workspace: Some(ws.clone()),
        decay_alpha: None,
        rel_depths: None,
    };
    let pipe = pipe_c.traverse(pipe_req).await;
    let bolt = bolt_c.traverse(bolt_req).await;
    log_pair("traverse", &pipe, &bolt);
    let _ = bolt_c.delete_workspace(&ws).await;

    assert!(pipe.ok && bolt.ok);
    let pipe_ids = chunk_ids(&pipe);
    let bolt_ids = chunk_ids(&bolt);
    assert_eq!(pipe_ids, bolt_ids, "traverse chunk IDs must match");
    // structural: chunks have the same keys
    for chunks in [&pipe.data["chunks"], &bolt.data["chunks"]] {
        for c in chunks.as_array().unwrap_or(&vec![]) {
            for key in ["id", "path", "symbol", "language", "graph_rank", "graph_score", "graph_depth", "graph_bucket"] {
                assert!(c.get(key).is_some(), "traverse chunk missing key {key}: {c}");
            }
        }
    }
}

// ── 7. relate ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_07_relate() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    // Unique prefix so we don't disturb any production Entity rows.
    let pfx_p = format!("parity_rel_pipe_{}", std::process::id());
    let pfx_b = format!("parity_rel_bolt_{}", std::process::id());

    let pairs_p = vec![EntityPair::new(format!("{pfx_p}_a"), format!("{pfx_p}_b"))];
    let pairs_b = vec![EntityPair::new(format!("{pfx_b}_a"), format!("{pfx_b}_b"))];

    let pipe = pipe_c.relate("CALLS", pairs_p).await;
    let bolt = bolt_c.relate("CALLS", pairs_b).await;
    log_pair("relate", &pipe, &bolt);

    // Cleanup (best-effort — bolt only, since the pipe API is broken).
    let _ = bolt_c
        .unrelate(
            "CALLS",
            vec![EntityPair::new(format!("{pfx_p}_a"), format!("{pfx_p}_b"))],
        )
        .await;
    let _ = bolt_c
        .unrelate(
            "CALLS",
            vec![EntityPair::new(format!("{pfx_b}_a"), format!("{pfx_b}_b"))],
        )
        .await;

    assert!(pipe.ok && bolt.ok, "both relate envelopes must report ok");
    assert_eq!(
        pipe.data["written"], bolt.data["written"],
        "relate.written count must match — Python's /relate route expects 'triples' \
         but the Rust pipe client sends 'pairs', so the server returns written=0 \
         while bolt actually writes the edge"
    );
}

// ── 8. unrelate ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_08_unrelate() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let pfx = format!("parity_unrel_{}", std::process::id());

    // Seed an edge via bolt so unrelate has something to drop.
    let _ = bolt_c
        .relate(
            "CALLS",
            vec![EntityPair::new(format!("{pfx}_a"), format!("{pfx}_b"))],
        )
        .await;

    let pipe = pipe_c
        .unrelate(
            "CALLS",
            vec![EntityPair::new(format!("{pfx}_a"), format!("{pfx}_b"))],
        )
        .await;
    // Re-seed for the bolt half — pipe's call should have been a no-op
    // but be defensive.
    let _ = bolt_c
        .relate(
            "CALLS",
            vec![EntityPair::new(format!("{pfx}_a"), format!("{pfx}_b"))],
        )
        .await;
    let bolt = bolt_c
        .unrelate(
            "CALLS",
            vec![EntityPair::new(format!("{pfx}_a"), format!("{pfx}_b"))],
        )
        .await;
    log_pair("unrelate", &pipe, &bolt);

    assert!(pipe.ok && bolt.ok);
    assert_eq!(
        pipe.data["deleted"], bolt.data["deleted"],
        "unrelate.deleted must match — same API-mismatch bug as relate"
    );
}

// ── 9. multihop ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_09_multihop() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let ws = test_workspace("mh");
    let _ = bolt_c.delete_workspace(&ws).await;

    let _ = bolt_c
        .upsert(vec![
            json!({
                "id": format!("{ws}-mh1"), "path": "mh1.py", "workspace": ws,
                "entities": ["mh_seed", "mh_extra_a"],
            }),
            json!({
                "id": format!("{ws}-mh2"), "path": "mh2.py", "workspace": ws,
                "entities": ["mh_seed", "mh_extra_b"],
            }),
        ])
        .await;

    let pipe = pipe_c.multihop(vec!["mh_seed".into()], 1, 10).await;
    let bolt = bolt_c.multihop(vec!["mh_seed".into()], 1, 10).await;
    log_pair("multihop", &pipe, &bolt);
    let _ = bolt_c.delete_workspace(&ws).await;

    assert!(pipe.ok && bolt.ok);
    let pipe_ids = chunk_ids(&pipe);
    let bolt_ids = chunk_ids(&bolt);
    assert_eq!(
        pipe_ids, bolt_ids,
        "multihop chunk IDs must match — guards against the Python \
         field-name bug (start vs entities) creeping back in"
    );
    // expanded_entities key must be present in both.
    assert!(pipe.data.get("expanded_entities").is_some());
    assert!(bolt.data.get("expanded_entities").is_some());
}

// ── 10. upsert_edge ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_10_upsert_edge() {
    let pipe_c = pipe_client();
    let bolt_c = bolt_client();
    let pfx = format!("parity_uedge_{}", std::process::id());

    let pipe = pipe_c
        .upsert_edge(
            &format!("{pfx}_s_p"),
            "MENTIONS",
            &format!("{pfx}_t_p"),
            1.0,
        )
        .await;
    let bolt = bolt_c
        .upsert_edge(
            &format!("{pfx}_s_b"),
            "MENTIONS",
            &format!("{pfx}_t_b"),
            1.0,
        )
        .await;
    log_pair("upsert_edge", &pipe, &bolt);

    assert_eq!(
        pipe.ok, bolt.ok,
        "upsert_edge ok-status must match — Python service has no \
         /upsert_edge route so pipe returns http_404 while bolt succeeds"
    );
}

// ── 11. stats ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "parity test — requires Neo4j + wylde-memgraph service live"]
async fn parity_11_stats() {
    let pipe = pipe_client().stats().await;
    let bolt = bolt_client().stats().await;
    log_pair("stats", &pipe, &bolt);

    assert!(pipe.ok && bolt.ok);
    // Both must surface the same five counts. Absolute values can
    // differ by 1-2 if concurrent activity is touching the DB between
    // calls, so we structural-check key presence + i64 type.
    for key in [
        "entities",
        "chunks",
        "mentions",
        "communities",
        "typed_relationships",
    ] {
        assert!(
            pipe.data.get(key).and_then(Value::as_i64).is_some(),
            "pipe.stats missing {key}: {}",
            pipe.data
        );
        assert!(
            bolt.data.get(key).and_then(Value::as_i64).is_some(),
            "bolt.stats missing {key}: {}",
            bolt.data
        );
    }
    // Counts should be close — within 5 of each other on a quiesced DB.
    // Not asserted because parallel test runs make the bound fragile;
    // we log instead so the operator can eyeball it.
    eprintln!(
        "stats deltas: \
         entities pipe={} bolt={}, \
         chunks pipe={} bolt={}, \
         mentions pipe={} bolt={}",
        pipe.data["entities"],
        bolt.data["entities"],
        pipe.data["chunks"],
        bolt.data["chunks"],
        pipe.data["mentions"],
        bolt.data["mentions"],
    );
}

// ── shared helpers ────────────────────────────────────────────────────

fn chunk_ids(reply: &Reply) -> Vec<String> {
    let mut ids: Vec<String> = reply
        .data
        .get("chunks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}
