//! Slice 0b acceptance: every relocated `workspaces.*` verb round-trips over
//! the real `wylde-workspaces` pipe through the shared client.
//!
//! Spawns the service binary on an isolated pipe + isolated `WYLDE_DATA_DIR`
//! (so it never touches a running prod instance or its data), then drives the
//! registry / persona / RAG verbs end-to-end via `WorkspacesClient` and
//! asserts the reply shapes. The `rag_query` / `reindex` calls run against an
//! empty workspace folder so no live embedder / sidecar is needed — they
//! exercise the verb plumbing + the fail-soft / empty-index paths.
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wylde_workspaces_client::WorkspacesClient;

fn unique_service_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wylde-workspaces-verbs-{}-{}", std::process::id(), nanos)
}

/// Spawn the service on `service_name`, with its data dir pointed at
/// `data_dir`. Returns the child so the caller can shut it down.
fn spawn_service(service_name: &str, data_dir: &std::path::Path, wylde_root: &std::path::Path) -> Child {
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

/// Wait until the service answers `ping`, or panic past the deadline.
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
async fn all_relocated_verbs_round_trip_over_the_new_pipe() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    // Two empty project folders to register as workspaces.
    let proj_a = tempfile::tempdir().expect("proj a");
    let proj_b = tempfile::tempdir().expect("proj b");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(r"\\.\pipe\{service_name}")));
    await_ready(&client, &mut child).await;

    // ── create A → registered + active ──────────────────────────────────
    let a = client
        .create(&proj_a.path().to_string_lossy(), Some("Proj A"))
        .await
        .expect("create A");
    let a_id = a["id"].as_str().expect("A id").to_owned();
    assert_eq!(a["name"], "Proj A");

    // ── create B → active flips to B ────────────────────────────────────
    let b = client
        .create(&proj_b.path().to_string_lossy(), None)
        .await
        .expect("create B");
    let b_id = b["id"].as_str().expect("B id").to_owned();

    // ── list_mru → newest-first [B, A], active = B ──────────────────────
    let listed = client.list_mru().await.expect("list_mru");
    let ids: Vec<&str> = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![b_id.as_str(), a_id.as_str()], "newest-first MRU");
    assert_eq!(listed["active_id"], b_id);

    // ── set_active A → active flips back ────────────────────────────────
    let sw = client.set_active(&a_id).await.expect("set_active A");
    assert_eq!(sw["active_id"], a_id);

    // ── update A (rag_enabled=false) ────────────────────────────────────
    let upd = client
        .update(serde_json::json!({ "workspace_id": a_id, "rag_enabled": false }))
        .await
        .expect("update A");
    assert_eq!(upd["rag_enabled"], false);

    // ── set_persona A → enables persona ─────────────────────────────────
    let sp = client.set_persona(&a_id, "Answer tersely.").await.expect("set_persona");
    assert_eq!(sp["ok"], true);

    // ── rag_query A → fail-soft empty hits (no index) ───────────────────
    let rq = client.rag_query(&a_id, "anything", Some(3)).await.expect("rag_query");
    assert_eq!(rq["hits"], serde_json::json!([]));

    // ── reindex A → empty folder → ok, zero files ───────────────────────
    let rx = client.reindex(&a_id).await.expect("reindex");
    assert_eq!(rx["ok"], true, "reindex of empty folder ok: {rx}");
    assert_eq!(rx["file_count"], 0);

    // ── delete A → gone, only B remains ─────────────────────────────────
    let del = client.delete(&a_id).await.expect("delete A");
    assert_eq!(del["ok"], true);

    // Read back with a FRESH client: the first `client` cached `list_mru`
    // (30s read-through TTL per the verb table), so its view is intentionally
    // stale right after a mutation. A new client has an empty cache.
    let fresh = WorkspacesClient::new(std::path::PathBuf::from(format!(r"\\.\pipe\{service_name}")));
    let after = fresh.list_mru().await.expect("list_mru after delete");
    let remaining: Vec<&str> = after["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(remaining, vec![b_id.as_str()], "only B survives");

    // ── deleted A is truly gone: set_active(A) → not_found over the pipe ─
    match fresh.set_active(&a_id).await {
        Err(e) => assert_eq!(e.code, "not_found", "deleted A should be not_found, got {e:?}"),
        Ok(v) => panic!("expected not_found for deleted A, got ok: {v}"),
    }

    // ── unknown id → application error surfaces over the pipe ────────────
    match fresh.set_active("nope-000000").await {
        Err(e) => assert_eq!(e.code, "not_found", "expected not_found, got {e:?}"),
        Ok(v) => panic!("expected not_found error, got ok: {v}"),
    }

    shutdown(child);
}
