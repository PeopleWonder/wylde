//! wylde-extension-bridge service entry point.
//!
//! Boots the manifest, registers the 9-action surface (plus
//! extensions.dispatch back-compat alias), opens the pipe at
//! `\\.\pipe\wylde-extension-bridge`, discovers and spawns any
//! `enabled=true` MCP-server extensions, and serves until Ctrl-C.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-extension-bridge";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-extension-bridge: starting (rust impl)");

    let cfg = wylde_extension_bridge::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "extensions",
        "Extension bridge — Rust MCP-server host. Spawns child MCP servers \
         (any language) and bridges 9 first-class ext.* actions over the \
         IPC pipe. Replaces the Python importlib-based Extensions.extension_bridge.",
        json!({
            "wylde_extension_bridge": {
                "actions": [
                    "ext.list",
                    "ext.get",
                    "ext.enable",
                    "ext.disable",
                    "ext.tools.list",
                    "ext.tools.call",
                    "ext.resources.list",
                    "ext.health",
                    "ext.restart",
                    "ext.events",
                    "extensions.dispatch",
                ],
                "mcp_spec_version": wylde_extension_bridge::config::MCP_SPEC_VERSION,
                "mcp_spec_version_prev": wylde_extension_bridge::config::MCP_SPEC_VERSION_PREV,
                "extensions_dir": cfg.extensions_dir.display().to_string(),
            },
            "dashboard": {
                "label": "Extension Bridge",
                "icon": "puzzle",
                "color": "green",
            },
        }),
        Some("rust:wylde-extension-bridge"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    wylde_extension_bridge::service::install();
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-extension-bridge: action contract write failed: {e}");
    }

    // Discover + spawn enabled extensions (best-effort; per-extension
    // failures get logged + surfaced via ext.list).
    wylde_extension_bridge::service::bootstrap().await;

    tracing::info!(
        "wylde-extension-bridge: actions registered; opening pipe at \\\\.\\pipe\\wylde-extension-bridge"
    );

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-extension-bridge: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-extension-bridge: ctrl-c received, shutting down");
        }
    }

    // Reap children before exit.
    wylde_extension_bridge::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-extension-bridge: mark_stopped failed: {e}");
    }
    Ok(())
}
