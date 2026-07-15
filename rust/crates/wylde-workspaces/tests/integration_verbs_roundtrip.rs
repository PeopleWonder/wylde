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
use std::time::{Duration, Instant};

use wylde_workspaces_client::WorkspacesClient;

/// A collision-proof service (and pipe) name. Tests in this file run
/// concurrently in one process (same pid), so a `pid + timestamp` name can tie
/// on a shared clock tick and — because the IPC server sets no
/// `first_pipe_instance` — two services would share one pipe and cross-talk.
/// The random `uuid` removes that tie (same convention as
/// `integration_rag_indexer.rs`; see #29).
fn unique_service_name() -> String {
    format!(
        "wylde-workspaces-verbs-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Spawn the service on `service_name`, with its data dir pointed at
/// `data_dir`. Returns the child so the caller can shut it down.
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
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));
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
    let sp = client
        .set_persona(&a_id, "Answer tersely.")
        .await
        .expect("set_persona");
    assert_eq!(sp["ok"], true);

    // ── rag_query A → fail-soft empty hits (no index) ───────────────────
    let rq = client
        .rag_query(&a_id, "anything", Some(3))
        .await
        .expect("rag_query");
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
    let fresh = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));
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
        Err(e) => assert_eq!(
            e.code, "not_found",
            "deleted A should be not_found, got {e:?}"
        ),
        Ok(v) => panic!("expected not_found for deleted A, got ok: {v}"),
    }

    // ── unknown id → application error surfaces over the pipe ────────────
    match fresh.set_active("nope-000000").await {
        Err(e) => assert_eq!(e.code, "not_found", "expected not_found, got {e:?}"),
        Ok(v) => panic!("expected not_found error, got ok: {v}"),
    }

    shutdown(child);
}

/// Slice 0c acceptance: the relocated notes + workspace-conversation verbs
/// round-trip over the real pipe through the shared client.
#[tokio::test]
async fn slice_0c_notes_and_conversations_round_trip() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    let proj = tempfile::tempdir().expect("proj");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));
    await_ready(&client, &mut child).await;

    // Register a workspace to scope the notes / conversations to.
    let ws = client
        .create(&proj.path().to_string_lossy(), Some("Proj"))
        .await
        .expect("create");
    let ws_id = ws["id"].as_str().expect("ws id").to_owned();

    // ── notes: add → list → update → delete (embedder is absent in the ──
    //    test env, so notes persist with an empty embedding — non-fatal). ─
    let added = client
        .notes_add(&ws_id, "uses tokio")
        .await
        .expect("notes.add");
    let note_id = added["id"].as_str().expect("note id").to_owned();
    assert_eq!(added["text"], "uses tokio");

    let listed = client.notes_list(&ws_id).await.expect("notes.list");
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["notes"][0]["id"], note_id);

    let updated = client
        .notes_update(&ws_id, &note_id, "uses cargo")
        .await
        .expect("notes.update");
    assert_eq!(updated["text"], "uses cargo");

    // update of an unknown id surfaces not_found over the pipe.
    match client.notes_update(&ws_id, "ghost", "x").await {
        Err(e) => assert_eq!(e.code, "not_found"),
        Ok(v) => panic!("expected not_found, got {v}"),
    }

    let searched = client
        .notes_search(&ws_id, "anything", Some(5))
        .await
        .expect("notes.search");
    assert_eq!(searched["count"], 1, "the one note ranks by recency");

    // propose returns a non-persisted candidate.
    let proposed = client
        .notes_propose(&ws_id, "prefers Rust")
        .await
        .expect("propose");
    assert_eq!(proposed["candidate"]["text"], "prefers Rust");
    assert_eq!(
        client.notes_list(&ws_id).await.unwrap()["count"],
        1,
        "propose did not persist"
    );

    let deleted = client
        .notes_delete(&ws_id, &note_id)
        .await
        .expect("notes.delete");
    assert_eq!(deleted["ok"], true);
    assert_eq!(client.notes_list(&ws_id).await.unwrap()["count"], 0);

    // ── workspace conversations: seed a per-workspace file on disk, then ─
    //    list / get / delete it over the pipe. ────────────────────────────
    let conv_dir = data_dir
        .path()
        .join("workspaces")
        .join(&ws_id)
        .join("conversations");
    std::fs::create_dir_all(&conv_dir).expect("conv dir");
    std::fs::write(
        conv_dir.join("c1.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "id": "c1", "title": "WS chat", "updated_at": 5,
            "messages": [{"role": "user", "content": "hi"}],
            "working_memory": [], "workspace_id": ws_id,
        }))
        .unwrap(),
    )
    .expect("seed conv");

    let convs = client.conversations_list(&ws_id).await.expect("conv.list");
    assert_eq!(convs["count"], 1);
    assert_eq!(convs["conversations"][0]["id"], "c1");

    let got = client
        .conversations_get(&ws_id, "c1")
        .await
        .expect("conv.get");
    assert_eq!(got["title"], "WS chat");

    // not_found surfaces over the pipe.
    match client.conversations_get(&ws_id, "ghost").await {
        Err(e) => assert_eq!(e.code, "not_found"),
        Ok(v) => panic!("expected not_found, got {v}"),
    }

    // Slice E parity: push a computed summary+embedding and read it back. The
    // derived fields land on the doc (what the scoped search ranks on) and
    // updated_at is untouched.
    client
        .conversations_refresh_summary(
            &ws_id,
            "c1",
            "Greeting exchange.",
            &["greeting".to_string()],
            &[0.1_f32, 0.2, 0.3],
            1,
        )
        .await
        .expect("conv.refresh_summary");
    let summarised = client
        .conversations_get(&ws_id, "c1")
        .await
        .expect("conv.get");
    assert_eq!(summarised["auto_summary"], "Greeting exchange.");
    assert_eq!(summarised["embedding"].as_array().unwrap().len(), 3);
    assert_eq!(summarised["updated_at"], 5, "re-summary must not reorder");

    let del = client
        .conversations_delete(&ws_id, "c1")
        .await
        .expect("conv.delete");
    assert_eq!(del["ok"], true);
    assert_eq!(client.conversations_list(&ws_id).await.unwrap()["count"], 0);

    shutdown(child);
}
