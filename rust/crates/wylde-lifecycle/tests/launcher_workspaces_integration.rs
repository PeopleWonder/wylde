//! Launcher integration for `wylde-workspaces` (Thought Bubble System,
//! Slice A).
//!
//! Exercises the REAL daemon-managed spawn/stop wiring end-to-end:
//! [`start_workspaces`] forks the actual `wylde-workspaces.exe`, the test
//! confirms the pipe binds and answers the IPC server's built-in
//! `/__ping__` liveness method within 5s of launch (the same probe
//! `service.health` uses), then [`stop_workspaces`] sends the graceful
//! CTRL_BREAK and the test confirms the pipe is gone.
//!
//! `#[ignore]`: needs the `wylde-workspaces` binary built. Run with:
//!
//! ```bash
//! cargo build -p wylde-workspaces
//! cargo test -p wylde-lifecycle --test launcher_workspaces_integration -- --ignored --nocapture
//! ```
//!
//! Self-contained — no Memgraph/Ollama/tree-sitter needed: the service
//! binds its pipe and answers pings regardless of whether its ingest
//! upstreams are up (ingest only reaches them when a workspace is created).
//! Skips (passes) cleanly if a production `wylde-workspaces` is already
//! bound, so it never disturbs a running stack.

#![cfg(windows)]

use std::time::{Duration, Instant};

use serde_json::Value;
use wylde_lifecycle::registry::pipe_alive;
use wylde_lifecycle::state::service_name;
use wylde_lifecycle::state::services::{rust_binary_path, start_workspaces, stop_workspaces};
use wylde_shared::ipc::send_with_verb;

/// Probe `/__ping__` (the Rust IPC server's built-in liveness primitive —
/// the exact method `control::service.health` probes) on the workspaces
/// pipe. Returns true when the service replies ok.
async fn ping_ok() -> bool {
    let reply = send_with_verb(
        service_name::WORKSPACES,
        "/__ping__",
        "GET",
        Value::Null,
        Duration::from_secs(2),
    )
    .await;
    reply.ok
}

#[tokio::test]
#[ignore = "requires the wylde-workspaces binary built (cargo build -p wylde-workspaces)"]
async fn daemon_starts_workspaces_health_probe_then_clean_stop() {
    // Resolve the binary the launcher would spawn. If it isn't built, skip
    // — the test can't fork what doesn't exist.
    let Some(bin) = rust_binary_path(service_name::WORKSPACES) else {
        eprintln!(
            "[skip] no wylde-workspaces binary found — build with \
             `cargo build -p wylde-workspaces` first"
        );
        return;
    };
    eprintln!("[launcher] resolved binary: {}", bin.display());

    // Don't disturb a production instance: if the pipe is already bound,
    // start_workspaces would short-circuit (already-alive) and stop would
    // be a no-op, making the result meaningless. Skip cleanly.
    if pipe_alive(Some(service_name::WORKSPACES)) {
        eprintln!("[skip] a wylde-workspaces pipe is already bound (prod up?) — not disturbing it");
        return;
    }

    // ── start: the real daemon-managed spawn path ───────────────────────
    start_workspaces()
        .await
        .expect("start_workspaces should spawn the binary");

    // ── health probe: pipe binds + answers /__ping__ within 5s ───────────
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut healthy = false;
    while Instant::now() < deadline {
        if pipe_alive(Some(service_name::WORKSPACES)) && ping_ok().await {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let elapsed = Instant::now().saturating_duration_since(deadline - Duration::from_secs(5));
    eprintln!("[launcher] health probe healthy={healthy} after {elapsed:?}");

    // ── stop: graceful CTRL_BREAK teardown ───────────────────────────────
    // Always attempt the stop, even on a failed health probe, so we never
    // leak the child.
    let stop_result = stop_workspaces().await;

    assert!(
        healthy,
        "wylde-workspaces pipe did not become healthy within 5s of launch"
    );
    stop_result.expect("stop_workspaces should tear the child down cleanly");

    // ── confirm the pipe is gone (give the OS a beat to retract it) ──────
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut gone = false;
    while Instant::now() < deadline {
        if !pipe_alive(Some(service_name::WORKSPACES)) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        gone,
        "wylde-workspaces pipe should be gone after stop_workspaces"
    );
    eprintln!("[launcher] clean stop confirmed — pipe retracted");
}
