//! Slice 0a acceptance test: spawn the `wylde-workspaces` binary on an
//! isolated pipe, round-trip the `ping` verb through the shared
//! `wylde-workspaces-client`, and verify a clean graceful shutdown.
//!
//! The service is spawned with an isolated pipe name (so it never clashes
//! with a running prod instance) and an isolated `WYLDE_ROOT` (so the
//! manifest / action-contract writes land in a tempdir, not the repo). The
//! `WYLDE_WORKSPACES_SHUTDOWN_ON_STDIN_EOF` gate turns closing the child's
//! stdin into a graceful, exit-code-0 shutdown — the cross-platform-clean
//! alternative to console control events on Windows.

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wylde_workspaces_client::WorkspacesClient;

/// A collision-proof service/pipe name for this test run. A `pid + timestamp`
/// name can tie between the file's concurrently-running tests (same pid, same
/// clock tick); the IPC server sets no `first_pipe_instance`, so two services
/// on one name share the pipe and cross-talk. The random `uuid` removes the
/// tie (same convention as `integration_rag_indexer.rs`; see #29).
fn unique_service_name() -> String {
    format!(
        "wylde-workspaces-it-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

#[tokio::test]
async fn ping_round_trips_and_shuts_down_cleanly() {
    let service_name = unique_service_name();
    let tmp = tempfile::tempdir().expect("tempdir for WYLDE_ROOT");

    // 1. Spawn the service binary on the isolated pipe. Cargo guarantees the
    //    binary is built and exposes its path via CARGO_BIN_EXE_<name>.
    let bin = env!("CARGO_BIN_EXE_wylde-workspaces");
    let mut child = Command::new(bin)
        .env("WYLDE_WORKSPACES_PIPE_NAME", &service_name)
        .env("WYLDE_ROOT", tmp.path())
        .env("WYLDE_WORKSPACES_SHUTDOWN_ON_STDIN_EOF", "1")
        // Keep transport on pipes regardless of the ambient environment.
        .env("WYLDE_TRANSPORT", "pipe")
        .env_remove("WYLDE_IPC_DISABLE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wylde-workspaces binary");

    // 2. Connect via the shared client. ping()'s internal connect-retry
    //    (2s budget per attempt) bridges the brief startup race, so a couple
    //    of attempts within a generous window is plenty.
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));

    let deadline = Instant::now() + Duration::from_secs(10);
    let resp = loop {
        match client.ping().await {
            Ok(r) => break r,
            Err(e) if Instant::now() < deadline => {
                // Service may still be binding; brief pause then retry.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = e;
            }
            Err(e) => {
                let _ = child.kill();
                panic!("ping never succeeded within budget: {e}");
            }
        }
    };

    // 3. Assert the reply shape.
    assert!(resp.ok, "ping ok flag");
    assert_eq!(resp.service, "wylde-workspaces");
    assert_eq!(resp.version, env!("CARGO_PKG_VERSION"));

    // 4. Graceful shutdown: dropping the child's stdin closes it → the
    //    service sees EOF → it exits cleanly with code 0.
    drop(child.stdin.take());

    // Wait for clean exit (poll try_wait up to a budget).
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() < exit_deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None => {
                let _ = child.kill();
                panic!("service did not exit after stdin EOF within budget");
            }
        }
    };

    assert!(
        status.success(),
        "service exited uncleanly: {status:?} (code {:?})",
        status.code()
    );
}
