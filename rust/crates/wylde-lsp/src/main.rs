//! wylde-lsp service entry point.
//!
//! Boots the manifest, registers the `lsp.*` action surface, opens the pipe at
//! `\\.\pipe\wylde-lsp`, and serves until Ctrl-C. Same shape as the other
//! greenfield Rust services. The rust-analyzer child is NOT spawned at startup
//! — it's lazily started on the first `lsp.open`, so an install without
//! rust-analyzer still runs this service cleanly (every verb reports
//! unavailable).

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-lsp";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-lsp: starting (rust-analyzer host)");

    let cfg = wylde_lsp::config::Config::get();

    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "core",
        "Optional rust-analyzer LSP host — diagnostics / completions / hover over lsp.* verbs.",
        json!({
            "wylde_lsp": {
                "actions": wylde_lsp::service::ALL_ACTIONS,
                "server": "rust-analyzer",
                "rust_analyzer": cfg.rust_analyzer,
                "optional": true,
            },
        }),
        Some("rust:wylde-lsp"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    wylde_lsp::service::install();

    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-lsp: action contract write failed: {e}");
    }

    tracing::info!(
        "wylde-lsp: actions registered; opening pipe at \\\\.\\pipe\\wylde-lsp"
    );

    if let Err(e) = ipc::serve(SERVICE_NAME, None).await {
        tracing::error!("wylde-lsp: serve() exited with error: {e}");
    }
    Ok(())
}
