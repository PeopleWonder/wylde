//! `wylde-workspaces` service entry point.
//!
//! Boots logging + the runtime manifest, registers the action surface
//! (Slice 0a: a single `ping` verb), opens the pipe at
//! `\\.\pipe\wylde-workspaces` (name overridable via
//! `WYLDE_WORKSPACES_PIPE_NAME`), and serves until a shutdown signal.
//! Same shape as `wylde-ollama/main.rs` — the standing service pattern.
//!
//! Shutdown is graceful: the accept loop is driven inside a `select!`
//! against Ctrl-C (the production signal) and an optional stdin-EOF watcher.
//! The latter is gated behind `WYLDE_WORKSPACES_SHUTDOWN_ON_STDIN_EOF` and
//! exists only so the integration test can close the child's stdin to get a
//! deterministic, clean (exit-code-0) shutdown on Windows without
//! console-control-event gymnastics. Production never sets that env var, so
//! a null/absent stdin can't trip an early exit.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-workspaces";

/// Env flag (test-only) that turns stdin EOF into a graceful shutdown
/// trigger. Unset in production.
const SHUTDOWN_ON_STDIN_EOF_ENV: &str = "WYLDE_WORKSPACES_SHUTDOWN_ON_STDIN_EOF";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-workspaces: starting (rust impl)");

    let cfg = wylde_workspaces::config::Config::get();

    // Runtime manifest so the lifecycle registry / dashboard can discover
    // the service once it's wired into the launcher (a later slice). The
    // `service_name` may be an isolated test name; the manifest pipe field
    // is derived from it, so it always matches the bound pipe.
    let manifest = ManifestWriter::write(
        &cfg.service_name,
        None,
        "core",
        "Workspace-scoped service — registry, persona, RAG, notes, workspace \
         conversations, code graph.",
        json!({
            "wylde_workspaces": {
                "actions": wylde_workspaces::action_dispatch::ALL_ACTIONS,
                "data_dir": cfg.data_dir.to_string_lossy(),
            },
        }),
        Some("rust:wylde-workspaces"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Idempotent first-startup data migration (Slice 0-migrate): relocate
    // pre-split workspace conversations from the flat harness store into the
    // new per-workspace bundle layout. Marker-gated, so re-runs are no-ops.
    // Runs ONLY here (the live service) — never from the harness fallback —
    // so production's flat store stays intact until go-live (Slice A).
    let report = wylde_workspaces::migration::run_pending();
    if !report.skipped {
        tracing::info!(
            "wylde-workspaces: migration v1 complete (moved={}, kept_standalone={}, errors={})",
            report.moved,
            report.kept_standalone,
            report.errors,
        );
    }

    // Populate the registry BEFORE serving so the first client connection
    // finds the `ping` handler.
    wylde_workspaces::action_dispatch::install();

    // Write the action contract for `wylde_check` / the cross-language
    // registry. Best-effort — a warning is fine.
    if let Err(e) = ipc::write_action_contract(&cfg.service_name, &cfg.wylde_root) {
        tracing::warn!("wylde-workspaces: action contract write failed: {e}");
    }

    let pipe = wylde_workspaces::ipc::pipe_path(&cfg.service_name);
    tracing::info!("wylde-workspaces: actions registered; opening pipe at {pipe}");

    let serve_fut = wylde_workspaces::ipc::serve(&cfg.service_name);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-workspaces: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-workspaces: ctrl-c received, shutting down");
        }
        _ = wait_for_stdin_eof_shutdown() => {
            tracing::info!("wylde-workspaces: stdin EOF received, shutting down");
        }
    }

    wylde_workspaces::action_dispatch::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-workspaces: mark_stopped failed: {e}");
    }
    Ok(())
}

/// Resolves when stdin reaches EOF, but ONLY when
/// [`SHUTDOWN_ON_STDIN_EOF_ENV`] is set. Otherwise it stays pending forever
/// so it never wins the `select!` in production (where stdin may be null or
/// closed by the lifecycle daemon).
async fn wait_for_stdin_eof_shutdown() {
    let enabled = std::env::var(SHUTDOWN_ON_STDIN_EOF_ENV)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if !enabled {
        // Never resolve.
        std::future::pending::<()>().await;
        return;
    }

    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 64];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) => return,   // EOF → trigger shutdown
            Ok(_) => continue, // drain any bytes; we only care about EOF
            Err(_) => return,  // treat a read error as EOF too
        }
    }
}
