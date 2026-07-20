//! Live end-to-end for the file watcher → per-file delta-upsert (Slice I).
//!
//! Drives the REAL watcher: a `notify` watch over a throwaway workspace folder
//! feeds the debouncer, which dispatches to the per-file delta path
//! (`treesitter.extract_entities` over the sidecar pipe → Neo4j over Bolt).
//! It walks the spec's exact scenario and reads the graph back over Bolt:
//!
//!   1. create `foo.rs` with `fn foo() -> i32 { 42 }`  → Entity `foo` appears
//!   2. modify it to `fn foo() -> i32 { bar() }`        → CALLS `foo → bar` appears
//!   3. delete it                                       → `foo` (this ws) is gone
//!
//! Every assertion is **workspace-scoped** (via the file's Chunk nodes) so the
//! globally-named `foo`/`bar` entities can't collide with anything else in the
//! graph, and the per-delta watcher-to-graph time is checked against the
//! <500ms ingest budget (the debounce quiet-window is excluded — it's the
//! deliberate coalescing wait, not processing).
//!
//! `#[ignore]`: needs the tree-sitter sidecar (`\\.\pipe\wylde-treesitter`)
//! and Neo4j (`bolt://127.0.0.1:7687`) live — e.g. after `wylde-lifecycle.exe`
//! booted them. Run with:
//!
//! ```bash
//! cargo test -p wylde-workspaces --test integration_watcher_delta -- --ignored --nocapture
//! ```
//!
//! Self-cleans (`delete_workspace`) so it never leaves rows in the graph.

#![cfg(windows)]

use std::time::{Duration, Instant};

use wylde_workspaces::graph::BoltClient;
use wylde_workspaces::registry;
use wylde_workspaces::watcher::{self, DeltaEvent};

/// A small debounce so the test doesn't wait the full 500ms per edit; the
/// watcher-to-graph budget is measured on the per-delta `took_ms`, which is
/// independent of this window.
const TEST_DEBOUNCE_MS: &str = "120";
/// The plan's per-file ingest budget (watcher → graph), in ms.
const BUDGET_MS: f64 = 500.0;

async fn graph() -> neo4rs::Graph {
    let uri = std::env::var("GRAPH_BOLT_URL").unwrap_or_else(|_| "bolt://127.0.0.1:7687".into());
    let cfg = neo4rs::ConfigBuilder::default()
        .uri(uri)
        .user(std::env::var("GRAPH_USER").unwrap_or_default())
        .password(std::env::var("GRAPH_PASSWORD").unwrap_or_default())
        .build()
        .expect("bolt config");
    neo4rs::Graph::connect(cfg).await.expect("connect neo4j")
}

async fn scalar(g: &neo4rs::Graph, q: neo4rs::Query) -> i64 {
    let mut rows = g.execute(q).await.expect("query");
    match rows.next().await {
        Ok(Some(row)) => row.get::<i64>("n").unwrap_or(0),
        _ => 0,
    }
}

/// Count, scoped to this workspace's chunks, of entities named `name`.
async fn entity_count(g: &neo4rs::Graph, ws: &str, name: &str) -> i64 {
    scalar(
        g,
        neo4rs::query(
            "MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity {name: $name}) \
             RETURN count(DISTINCT e) AS n",
        )
        .param("ws", ws.to_owned())
        .param("name", name.to_owned()),
    )
    .await
}

/// Count, scoped to this workspace, of `src -CALLS-> dst` edges.
async fn calls_count(g: &neo4rs::Graph, ws: &str, src: &str, dst: &str) -> i64 {
    scalar(
        g,
        neo4rs::query(
            "MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity {name: $src}) \
             -[r:CALLS]->(:Entity {name: $dst}) RETURN count(DISTINCT r) AS n",
        )
        .param("ws", ws.to_owned())
        .param("src", src.to_owned())
        .param("dst", dst.to_owned()),
    )
    .await
}

/// Wait for the next `delta_upsert_complete` event whose path ends with
/// `file_suffix` and matches `action`, returning its `took_ms`.
async fn await_delta(
    rx: &mut tokio::sync::broadcast::Receiver<DeltaEvent>,
    action: &str,
    file_suffix: &str,
) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "no {action} delta for {file_suffix} within budget"
        );
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) if ev.action == action && ev.path.ends_with(file_suffix) => {
                return ev.took_ms
            }
            Ok(Ok(_)) => continue,  // some other delta — keep waiting
            Ok(Err(_)) => continue, // lagged/closed — keep trying until deadline
            Err(_) => panic!("no {action} delta for {file_suffix} within budget"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live tree-sitter sidecar + Neo4j (run the stack first)"]
async fn watcher_create_modify_delete_round_trip_within_budget() {
    // ── Isolated data dir + small debounce so production state is untouched ──
    let data = tempfile::tempdir().expect("data tempdir");
    std::env::set_var("WYLDE_DATA_DIR", data.path());
    std::env::set_var("WYLDE_WORKSPACES_WATCH_DEBOUNCE_MS", TEST_DEBOUNCE_MS);

    let folder = tempfile::tempdir().expect("workspace tempdir");
    let folder_path = folder.path().to_string_lossy().into_owned();

    // Register + activate the workspace (folder is empty, so no initial index).
    let def = registry::create(&folder_path, Some("watcher-it")).unwrap();
    let ws = def.id.clone();
    eprintln!("[watcher-it] workspace {ws} @ {folder_path}");

    // Subscribe BEFORE arming so no completion event is missed.
    let mut rx = watcher::subscribe();
    watcher::enable();
    watcher::on_active_changed();
    assert_eq!(
        watcher::status().active_workspace.as_deref(),
        Some(ws.as_str()),
        "watcher should be active on the new workspace"
    );

    let g = graph().await;
    let file = folder.path().join("foo.rs");
    let mut budget_samples: Vec<f64> = Vec::new();

    // ── 1. CREATE: fn foo() -> i32 { 42 } → Entity foo appears ──────────────
    std::fs::write(&file, "fn foo() -> i32 { 42 }\n").unwrap();
    let took = await_delta(&mut rx, "upsert", "foo.rs").await;
    budget_samples.push(took);
    eprintln!("[watcher-it] create delta took {took:.1}ms");
    let foo = poll_until(|| entity_count(&g, &ws, "foo"), |n| n > 0).await;
    assert!(foo > 0, "expected Entity foo after create (got {foo})");

    // ── 2. MODIFY: body calls bar() → CALLS foo → bar appears ───────────────
    std::fs::write(&file, "fn foo() -> i32 { bar() }\n").unwrap();
    let took = await_delta(&mut rx, "upsert", "foo.rs").await;
    budget_samples.push(took);
    eprintln!("[watcher-it] modify delta took {took:.1}ms");
    let calls = poll_until(|| calls_count(&g, &ws, "foo", "bar"), |n| n > 0).await;
    assert!(
        calls > 0,
        "expected CALLS foo→bar after modify (got {calls})"
    );

    // ── 3. DELETE: foo (this workspace) is gone ─────────────────────────────
    std::fs::remove_file(&file).unwrap();
    let took = await_delta(&mut rx, "remove", "foo.rs").await;
    budget_samples.push(took);
    eprintln!("[watcher-it] delete delta took {took:.1}ms");
    let foo_after = poll_until(|| entity_count(&g, &ws, "foo"), |n| n == 0).await;
    assert_eq!(
        foo_after, 0,
        "expected foo gone after delete (got {foo_after})"
    );

    // ── Budget: every watcher-to-graph delta within 500ms ───────────────────
    let p_max = budget_samples.iter().cloned().fold(0.0_f64, f64::max);
    eprintln!(
        "[watcher-it] watcher-to-graph samples = {budget_samples:?} ms; max = {p_max:.1}ms / \
         target {BUDGET_MS}ms"
    );
    assert!(
        p_max < BUDGET_MS,
        "watcher-to-graph delta exceeded the {BUDGET_MS}ms budget: {p_max:.1}ms"
    );

    // ── Cleanup ─────────────────────────────────────────────────────────────
    watcher::stop();
    let del = BoltClient::new().delete_workspace(&ws).await;
    assert!(del.ok, "cleanup delete_workspace failed: {:?}", del.error);
    let remaining = scalar(
        &g,
        neo4rs::query("MATCH (c:Chunk {workspace: $ws}) RETURN count(c) AS n").param("ws", ws),
    )
    .await;
    assert_eq!(remaining, 0, "workspace chunks should be cleaned up");
}

/// Poll an async query until `pred` holds or a 5s deadline elapses, returning
/// the last value. Bridges the brief gap between the delta event and the
/// graph commit being visible to a fresh read.
async fn poll_until<F, Fut, P>(mut query: F, pred: P) -> i64
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = i64>,
    P: Fn(i64) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = query().await;
    while !pred(last) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        last = query().await;
    }
    last
}
