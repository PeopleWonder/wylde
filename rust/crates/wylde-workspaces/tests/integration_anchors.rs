//! Slice N-data acceptance: the `workspaces.anchors.*` verbs round-trip over
//! the real `wylde-workspaces` pipe through the shared client.
//!
//! Spawns the service binary on an isolated pipe + `WYLDE_DATA_DIR`, registers
//! a workspace, then drives the anchor data API end-to-end and asserts reply
//! shapes — create / list / find_by_token / find_by_target / list_under /
//! update / propose / delete, plus the duplicate-identifier collision.
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use wylde_workspaces_client::WorkspacesClient;

/// A collision-proof service/pipe name. A `pid + timestamp` name can tie
/// between this file's concurrently-running tests (same pid, same clock tick);
/// the IPC server sets no `first_pipe_instance`, so two services on one name
/// share the pipe and cross-talk. The random `uuid` removes the tie (same
/// convention as `integration_rag_indexer.rs`; see #29).
fn unique_service_name() -> String {
    format!(
        "wylde-workspaces-anchors-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

fn spawn_service(
    service_name: &str,
    data_dir: &std::path::Path,
    wylde_root: &std::path::Path,
) -> Child {
    let bin = env!("CARGO_BIN_EXE_wylde-workspaces");
    Command::new(bin)
        .env("WYLDE_WORKSPACES_PIPE_NAME", service_name)
        .env("WYLDE_DATA_DIR", data_dir)
        .env("WYLDE_ROOT", wylde_root)
        .env("WYLDE_WORKSPACES_SHUTDOWN_ON_STDIN_EOF", "1")
        .env("WYLDE_TRANSPORT", "pipe")
        .env_remove("WYLDE_IPC_DISABLE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wylde-workspaces binary")
}

async fn await_ready(client: &WorkspacesClient, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client.ping().await {
            Ok(_) => return,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                let _ = child.kill();
                panic!("service never became ready: {e}");
            }
        }
    }
}

fn shutdown(mut child: Child) {
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return;
            }
        }
    }
}

#[tokio::test]
async fn anchor_verbs_round_trip_over_the_new_pipe() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    let proj = tempfile::tempdir().expect("proj");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let pipe = std::path::PathBuf::from(format!(r"\\.\pipe\{service_name}"));
    let client = WorkspacesClient::new(pipe.clone());
    await_ready(&client, &mut child).await;

    // Register a workspace to scope the anchors to.
    let ws = client
        .create(&proj.path().to_string_lossy(), Some("Proj"))
        .await
        .expect("create workspace");
    let ws_id = ws["id"].as_str().expect("ws id").to_owned();

    // ── create a concept anchor + a code-symbol anchor + a child ─────────
    let parent = client
        .anchors_create(json!({
            "workspace_id": ws_id, "identifier": "migration_pattern", "kind": "concept",
            "target": { "type": "concept", "text": "moving code incrementally" },
            "description": "the family of incremental-migration patterns",
        }))
        .await
        .expect("create parent");
    assert_eq!(parent["identifier"], "migration_pattern");
    assert_eq!(parent["scope"]["scope"], "workspace");
    assert_eq!(parent["scope"]["workspace_id"], ws_id);

    client
        .anchors_create(json!({
            "workspace_id": ws_id, "identifier": "strangler_fig_pattern", "kind": "concept",
            "target": { "type": "concept", "text": "wrap + replace" },
            "description": "the strangler-fig migration",
            "parent_anchor": "migration_pattern",
        }))
        .await
        .expect("create child");

    client
        .anchors_create(json!({
            "workspace_id": ws_id, "identifier": "run_pipeline", "kind": "code_symbol",
            "target": { "type": "code_symbol", "symbol_id": "orchestrator::run" },
            "description": "the ingest entry point",
        }))
        .await
        .expect("create symbol anchor");

    // ── list → 3 anchors ─────────────────────────────────────────────────
    let listed = client.anchors_list(&ws_id).await.expect("list");
    assert_eq!(listed["count"], 3, "three anchors: {listed}");

    // ── find_by_token (with braces) → 1 ─────────────────────────────────
    let by_token = client
        .anchors_find_by_token(&ws_id, "{{migration_pattern}}")
        .await
        .expect("find_by_token");
    assert_eq!(by_token["count"], 1);
    assert_eq!(by_token["token"], "migration_pattern");
    assert_eq!(by_token["anchors"][0]["identifier"], "migration_pattern");

    // ── find_by_target (inverse lookup, OI-20) → the symbol anchor ──────
    let by_target = client
        .anchors_find_by_target(&ws_id, "orchestrator::run")
        .await
        .expect("find_by_target");
    assert_eq!(by_target["count"], 1);
    assert_eq!(by_target["anchors"][0]["identifier"], "run_pipeline");

    // ── list_under (hierarchy, OI-19) → the child ───────────────────────
    let under = client
        .anchors_list_under(&ws_id, "migration_pattern")
        .await
        .expect("list_under");
    assert_eq!(under["count"], 1);
    assert_eq!(under["anchors"][0]["identifier"], "strangler_fig_pattern");

    // ── duplicate create → already_exists collision over the pipe ───────
    match client
        .anchors_create(json!({
            "workspace_id": ws_id, "identifier": "migration_pattern",
            "target": { "type": "concept", "text": "dup" },
        }))
        .await
    {
        // The shared client surfaces `code`/`message` (the structured
        // `details` are asserted in the api unit test, which sees the full
        // `IpcError`); the wire round-trip here pins the code.
        Err(e) => assert_eq!(e.code, "already_exists"),
        Ok(v) => panic!("expected already_exists, got ok: {v}"),
    }

    // ── update → patch description (fresh client: create is uncached but ─
    //    list has a 30s TTL, so read mutations through a fresh client). ───
    let updated = client
        .anchors_update(json!({
            "workspace_id": ws_id, "identifier": "run_pipeline",
            "description": "the ingest entry point (delta-aware)",
            "domain": "Storage",
        }))
        .await
        .expect("update");
    assert_eq!(
        updated["description"],
        "the ingest entry point (delta-aware)"
    );
    assert_eq!(updated["domain"], "Storage");

    // ── propose → non-persisted candidate gated on confidence ───────────
    let low = client
        .anchors_propose(json!({
            "workspace_id": ws_id, "identifier": "maybe_idea",
            "target": { "type": "concept", "text": "t" }, "confidence": 0.4,
        }))
        .await
        .expect("propose low");
    assert!(low["candidate"].is_null());
    assert_eq!(low["reason"], "low_confidence");

    let high = client
        .anchors_propose(json!({
            "workspace_id": ws_id, "identifier": "good_idea",
            "target": { "type": "concept", "text": "t" }, "confidence": 0.9,
        }))
        .await
        .expect("propose high");
    assert_eq!(high["candidate"]["identifier"], "good_idea");

    // propose did not persist — still 3 anchors (fresh client dodges the
    // list cache).
    let fresh = WorkspacesClient::new(pipe.clone());
    assert_eq!(
        fresh
            .anchors_list(&ws_id)
            .await
            .expect("list after propose")["count"],
        3,
        "propose must not persist"
    );

    // ── delete → 2 remain ────────────────────────────────────────────────
    let del = client
        .anchors_delete(&ws_id, "run_pipeline")
        .await
        .expect("delete");
    assert_eq!(del["ok"], true);
    let after = WorkspacesClient::new(pipe);
    assert_eq!(
        after.anchors_list(&ws_id).await.expect("list after delete")["count"],
        2
    );

    shutdown(child);
}

/// Slice N-data-aliases: create an anchor with human-friendly aliases over the
/// pipe, resolve it via a multi-word alias (canonical returned), and round-trip
/// the alias-driven promotion entry point.
#[tokio::test]
async fn alias_lookup_and_promotion_over_the_pipe() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    let proj = tempfile::tempdir().expect("proj");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let pipe = std::path::PathBuf::from(format!(r"\\.\pipe\{service_name}"));
    let client = WorkspacesClient::new(pipe.clone());
    await_ready(&client, &mut child).await;

    let ws = client
        .create(&proj.path().to_string_lossy(), Some("Proj"))
        .await
        .expect("create workspace");
    let ws_id = ws["id"].as_str().expect("ws id").to_owned();

    // Create an anchor carrying two aliases (one multi-word, un-normalised).
    let created = client
        .anchors_create(json!({
            "workspace_id": ws_id, "identifier": "set_active_graph_view", "kind": "concept",
            "target": { "type": "concept", "text": "switch the graph panel view" },
            "description": "switches the graph panel to the active workspace view",
            "aliases": ["  set   active ", "graph view"],
        }))
        .await
        .expect("create with aliases");
    assert_eq!(created["aliases"][0], "set active", "normalised on write");
    assert_eq!(created["aliases"][1], "graph view");

    // Resolve via a spaced, braced alias → the canonical anchor comes back.
    let by_alias = client
        .anchors_find_by_token(&ws_id, "{{set active}}")
        .await
        .expect("find_by_token via alias");
    assert_eq!(by_alias["count"], 1);
    assert_eq!(by_alias["token"], "set active");
    assert_eq!(
        by_alias["anchors"][0]["identifier"], "set_active_graph_view",
        "canonical identifier returned, not the alias"
    );

    // Promote via the alias → the whole anchor (all aliases) is handed back as
    // the promotion payload for the global landing point.
    let promo = client
        .anchors_promote_via_alias(&ws_id, "set_active_graph_view", "graph view")
        .await
        .expect("promote_via_alias");
    assert_eq!(promo["promote"], true);
    assert_eq!(promo["via_alias"], "graph view");
    assert_eq!(promo["anchor"]["identifier"], "set_active_graph_view");
    assert_eq!(promo["anchor"]["aliases"][0], "set active");
    assert_eq!(promo["anchor"]["aliases"][1], "graph view");

    // An alias that doesn't belong to the anchor → bad_request.
    match client
        .anchors_promote_via_alias(&ws_id, "set_active_graph_view", "not an alias")
        .await
    {
        Err(e) => assert_eq!(e.code, "bad_request"),
        Ok(v) => panic!("expected bad_request, got ok: {v}"),
    }

    shutdown(child);
}
