//! S1 (IDE plan P0.2) acceptance: the jailed `workspaces.fs.*` verbs
//! round-trip over the real `wylde-workspaces` pipe through the shared client.
//!
//! Spawns the service binary on an isolated pipe + `WYLDE_DATA_DIR`, registers
//! a tempdir workspace, then drives read / write / list_dir end-to-end and
//! asserts the jail rejects an escape over the wire.
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use wylde_workspaces_client::WorkspacesClient;

/// A collision-proof service/pipe name. A `pid + timestamp` name can tie
/// between this file's concurrently-running tests (same pid, same clock tick);
/// the IPC server sets no `first_pipe_instance`, so two services on one name
/// share the pipe and cross-talk. The random `uuid` removes the tie (same
/// convention as `integration_rag_indexer.rs`; see #29).
fn unique_service_name() -> String {
    format!(
        "wylde-workspaces-fs-{}-{}",
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
async fn fs_verbs_round_trip_over_the_pipe() {
    let service_name = unique_service_name();
    let data_dir = tempfile::tempdir().expect("data dir");
    let wylde_root = tempfile::tempdir().expect("wylde root");
    let proj = tempfile::tempdir().expect("proj");

    // Seed the workspace folder: a source file, a subdir, an ignored dir.
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").expect("seed main.rs");
    std::fs::create_dir(proj.path().join("src")).expect("mk src");
    std::fs::write(proj.path().join("src").join("lib.rs"), "pub fn f() {}\n").expect("seed lib.rs");
    std::fs::create_dir(proj.path().join("target")).expect("mk target");

    let mut child = spawn_service(&service_name, data_dir.path(), wylde_root.path());
    let client = WorkspacesClient::new(std::path::PathBuf::from(format!(
        r"\\.\pipe\{service_name}"
    )));
    await_ready(&client, &mut child).await;

    let ws = client
        .create(&proj.path().to_string_lossy(), Some("Proj"))
        .await
        .expect("create");
    let id = ws["id"].as_str().expect("ws id").to_owned();

    // ── list_dir root: one level, dirs first, ignored flags ──────────────
    let listed = client.fs_list_dir(&id, None).await.expect("list_dir");
    let entries = listed["entries"].as_array().expect("entries");
    let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"main.rs"), "root lists main.rs: {names:?}");
    assert!(names.contains(&"src"));
    assert!(names.contains(&"target"));
    assert!(!names.contains(&"lib.rs"), "lazy: nested file not listed");
    let target = entries.iter().find(|e| e["name"] == "target").unwrap();
    assert_eq!(target["ignored"], true, "target/ flagged ignored");

    // ── read an existing file ────────────────────────────────────────────
    let read = client.fs_read(&id, "main.rs").await.expect("read");
    assert_eq!(read["content"], "fn main() {}\n");
    assert_eq!(read["encoding"], "utf8");
    assert_eq!(read["binary"], false);
    let mtime = read["mtime"].as_f64().expect("mtime");

    // ── write (round-trips), then read back ──────────────────────────────
    let written = client
        .fs_write(&id, "main.rs", "fn main() { println!(\"hi\"); }\n", Some(mtime))
        .await
        .expect("write");
    assert!(written["size_bytes"].as_u64().unwrap() > 0);
    let reread = client.fs_read(&id, "main.rs").await.expect("reread");
    assert!(reread["content"].as_str().unwrap().contains("println"));

    // ── jail: a traversal escape is refused over the wire ────────────────
    match client.fs_read(&id, "../../etc/hosts").await {
        Err(e) => assert_eq!(e.code, "path_escape", "expected path_escape, got {e:?}"),
        Ok(v) => panic!("jail breach! read escaped: {v}"),
    }

    // ── conflict: a stale expected_mtime is refused ──────────────────────
    match client
        .fs_write(&id, "main.rs", "stale", Some(0.0))
        .await
    {
        Err(e) => assert_eq!(e.code, "conflict", "expected conflict, got {e:?}"),
        Ok(v) => panic!("stale write should conflict, got ok: {v}"),
    }

    shutdown(child);
}
