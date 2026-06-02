//! wylde-treesitter service entry point.
//!
//! Boots the manifest, registers the action surface (`languages` + `parse`
//! in Slice 1), opens the pipe at `\\.\pipe\wylde-treesitter`, and serves
//! until Ctrl-C. Same shape as `wylde-ollama/main.rs` — the Wylde user's
//! standing pattern for a greenfield (default-Rust, no Python fallback)
//! service. See `docs/plans/treesitter-sidecar.md`.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-treesitter";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-treesitter: starting (rust impl)");

    let cfg = wylde_treesitter::config::Config::get();

    // Advertise the linked grammars in the manifest so the dashboard /
    // `wylde_check` can see which languages this build parses.
    let grammars: Vec<&str> = wylde_treesitter::parser::REGISTRY
        .iter()
        .map(|g| g.name)
        .collect();

    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "core",
        "Tree-sitter sidecar — structural source parsing (chunk/entities/outline) over the pipe.",
        json!({
            "wylde_treesitter": {
                "actions": [
                    "treesitter.languages",
                    "treesitter.parse",
                    "treesitter.chunk",
                ],
                "grammars": grammars,
                "http_port": cfg.http_port,
            },
        }),
        Some("rust:wylde-treesitter"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register the actions on the process-wide registry. install() must
    // precede serve() so the registry is populated when the first pipe
    // client connects.
    wylde_treesitter::service::install();

    // Write the action contract on disk for `wylde_check` and the
    // cross-language registry. Path resolves to
    // `data/contracts/actions/wylde-treesitter.json` under WYLDE_ROOT.
    // (serve() also writes it; this is the belt-and-suspenders the other
    // services use so the artifact exists even if accept never opens.)
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-treesitter: action contract write failed: {e}");
    }

    tracing::info!(
        "wylde-treesitter: actions registered; opening pipe at \\\\.\\pipe\\wylde-treesitter and HTTP on 127.0.0.1:{}",
        cfg.http_port,
    );

    // Pipe server (harness) + loopback HTTP front door (N8N) run in parallel.
    // Either exiting — or ctrl-c — tears the process down.
    let pipe_fut = ipc::serve(SERVICE_NAME, None);
    let http_fut = wylde_treesitter::http::serve(cfg.http_port);
    tokio::select! {
        result = pipe_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-treesitter: pipe serve() exited with error: {e}");
            }
        }
        result = http_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-treesitter: HTTP serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-treesitter: ctrl-c received, shutting down");
        }
    }

    wylde_treesitter::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-treesitter: mark_stopped failed: {e}");
    }
    Ok(())
}
