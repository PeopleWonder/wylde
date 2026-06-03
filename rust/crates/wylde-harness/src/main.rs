//! wylde-harness service entry point.
//!
//! Boots the manifest, registers the five `chat.*` actions (4 of which
//! are streaming in slice 5.B), opens the pipe at `\\.\pipe\wylde-harness`,
//! serves until Ctrl-C. Same shape as `wylde-ollama/main.rs`.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-harness";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-harness: starting (rust impl, slice 7.B — long-term memory)");

    let cfg = wylde_harness::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "core",
        "Wylde harness — chat-turn driver (Rust port of Core/harness/turn, Phase 5).",
        json!({
            "wylde_harness": {
                "actions": [
                    "chat.run_turn",
                    "chat.start_turn",
                    "chat.cancel",
                    "chat.stream_turn",
                    "chat.stream_tools",
                    "memory.workspaces.list",
                    "memory.workspaces.recent",
                    "memory.workspaces.get",
                    "memory.workspaces.get_mru_limit",
                    "memory.workspaces.set_mru_limit",
                    "memory.workspaces.get_persona",
                    "memory.workspaces.set_persona",
                    "memory.workspaces.delete",
                ],
                "slice": "7.B",
                "implemented_actions": [
                    "chat.run_turn",
                    "chat.start_turn",
                    "chat.cancel",
                    "chat.stream_turn",
                    "chat.stream_tools",
                    "memory.workspaces.list",
                    "memory.workspaces.recent",
                    "memory.workspaces.get",
                    "memory.workspaces.get_mru_limit",
                    "memory.workspaces.set_mru_limit",
                    "memory.workspaces.get_persona",
                    "memory.workspaces.set_persona",
                    "memory.workspaces.delete",
                ],
                "stub_actions": [],
                "ollama_service": cfg.ollama_service.clone(),
                "default_model": cfg.default_model.clone(),
                "max_tool_loops": cfg.max_tool_loops,
            },
        }),
        Some("rust:wylde-harness"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    wylde_harness::service::install();

    // Slice 5a: populate the extension verb-resource overlay from
    // `wylde-extension-bridge` and follow its lifecycle bus. No-op unless
    // `WYLDE_HARNESS_VERB_TOOLS` is set — dark until the Slice-6 cutover.
    wylde_harness::tooling::resource::resources::extensions::spawn_sync_task(cfg);

    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-harness: action contract write failed: {e}");
    }

    tracing::info!(
        "wylde-harness: actions registered; opening pipe at \\\\.\\pipe\\wylde-harness"
    );

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-harness: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-harness: ctrl-c received, shutting down");
        }
    }

    wylde_harness::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-harness: mark_stopped failed: {e}");
    }
    Ok(())
}
