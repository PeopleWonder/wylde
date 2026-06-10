//! Slice 0d acceptance: the chat turn driver's workspace-context pickup
//! and its graceful-degradation path, exercised end-to-end over the real
//! `wylde-workspaces` pipe through the shared client.
//!
//! This is the exact call the harness chat turn driver makes once per turn
//! (`WorkspacesClient::gather_prompt`, see
//! `wylde_harness::turn::workspace_context`). The two tests cover:
//!
//! * **Full integration** — spawn the service, seed an active workspace
//!   with a persona + a note, and confirm `gather_prompt` surfaces both in
//!   the rendered system-prompt slots (the "richer with workspaces" path).
//! * **Negative** — kill the service mid-session and confirm the next call
//!   surfaces a transport failure (the driver degrades to base context),
//!   and that the per-pipe circuit breaker opens after 5 consecutive
//!   failures (fail-fast, scope v2 §7.4).
//!
//! Windows-only — IPC uses named pipes. No live embedder is needed: notes
//! persist with an empty embedding and rank by recency, so the note still
//! surfaces; RAG is empty (no index) and simply contributes nothing.

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wylde_workspaces_client::WorkspacesClient;

fn unique_service_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wylde-workspaces-gather-{}-{}", std::process::id(), nanos)
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

/// Stop the service and wait for the process to actually exit, so the pipe
/// is gone before the negative test issues its post-kill calls.
fn shutdown_and_wait(mut child: Child) {
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// **Full integration** — a live service surfaces the active workspace's
/// persona + notes in the gathered prompt slots.
#[tokio::test]
async fn gather_prompt_surfaces_active_workspace_persona_and_notes() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    let proj = tempfile::tempdir().expect("proj");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));
    await_ready(&client, &mut child).await;

    // Active workspace with a persona + one note. RAG is disabled for this
    // test: it needs a live embedder (absent here), and with RAG on + no
    // embedder the gather deliberately blows its 2s budget and degrades —
    // that hot-path behaviour is covered by the negative test. Disabling it
    // isolates the persona + notes pickup, which is what we assert here
    // (notes search is internally bounded to 1.2s, so it stays in budget).
    let ws = client
        .create(&proj.path().to_string_lossy(), Some("Proj"))
        .await
        .expect("create");
    let ws_id = ws["id"].as_str().expect("ws id").to_owned();
    client
        .update(serde_json::json!({ "workspace_id": ws_id, "rag_enabled": false }))
        .await
        .expect("disable rag");
    client
        .set_persona(&ws_id, "Answer as a terse Rust reviewer.")
        .await
        .expect("set_persona");
    client
        .notes_add(&ws_id, "the build uses tokio")
        .await
        .expect("notes.add");

    // The exact driver call — gather the rendered prompt slots.
    let slots = client
        .gather_prompt(&ws_id, "how do I run the build?")
        .await
        .expect("gather_prompt");

    assert!(
        slots.contains("# Workspace context"),
        "expected a workspace-context block, got: {slots:?}"
    );
    assert!(
        slots.contains("Answer as a terse Rust reviewer."),
        "persona must surface in slots: {slots:?}"
    );
    assert!(
        slots.contains("the build uses tokio"),
        "the workspace note must surface in slots: {slots:?}"
    );

    // An unknown workspace contributes nothing (empty slots, not an error)
    // — base context, byte-identical to a plain turn.
    let empty = client
        .gather_prompt("ghost-000000", "hi")
        .await
        .expect("gather_prompt unknown");
    assert_eq!(empty, "", "unknown workspace must yield empty slots");

    shutdown_and_wait(child);
}

/// **Negative** — once the service is gone, the next `gather_prompt`
/// surfaces a transport failure (the driver degrades to base context), and
/// the per-pipe breaker opens after 5 consecutive failures.
#[tokio::test]
async fn gather_prompt_degrades_then_trips_breaker_when_service_dies() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    let proj = tempfile::tempdir().expect("proj");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));
    await_ready(&client, &mut child).await;

    let ws = client
        .create(&proj.path().to_string_lossy(), Some("Proj"))
        .await
        .expect("create");
    let ws_id = ws["id"].as_str().expect("ws id").to_owned();
    // Healthy call works before the kill.
    client
        .gather_prompt(&ws_id, "hi")
        .await
        .expect("pre-kill gather");

    // Kill the service mid-session.
    shutdown_and_wait(child);

    // `gather_prompt` is NoRetry, so each post-kill call = exactly one
    // transport failure against the breaker. The breaker default opens
    // after 5 consecutive failures (scope v2 §7.4).
    for i in 1..=5 {
        let err = client
            .gather_prompt(&ws_id, "hi")
            .await
            .expect_err("service is dead — call must fail");
        assert!(
            err.transport,
            "call {i} after kill must be a transport failure (driver degrades): {err:?}"
        );
        assert_ne!(
            err.code, "breaker_open",
            "breaker must not be open yet at call {i}"
        );
    }

    // The 6th call fails fast on the now-open breaker — no pipe attempt.
    let tripped = client
        .gather_prompt(&ws_id, "hi")
        .await
        .expect_err("6th call must fail");
    assert_eq!(
        tripped.code, "breaker_open",
        "breaker must be open after 5 failures, got: {tripped:?}"
    );
}
