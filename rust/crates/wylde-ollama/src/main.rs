//! wylde-ollama service entry point.
//!
//! Boots the manifest, registers the action surface (10 actions), opens
//! the pipe at `\\.\pipe\wylde-ollama`, and serves until Ctrl-C. Same
//! shape as `wylde-vram-broker/main.rs` — the Wylde user's standing pattern.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-ollama";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-ollama: starting (rust impl)");

    let cfg = wylde_ollama::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "core",
        "Ollama inference proxy — single pipe surface for chat/embed/pull/show.",
        json!({
            "wylde_ollama": {
                "actions": [
                    "ollama.health",
                    "ollama.list_models",
                    "ollama.list_loaded",
                    "ollama.show",
                    "ollama.delete",
                    "ollama.eject",
                    "ollama.pull",
                    "ollama.chat",
                    "ollama.chat_stream",
                    "ollama.embed",
                ],
                "upstream_url": cfg.ollama_url.clone(),
                "pool_max_idle_per_host": cfg.pool_max_idle_per_host,
                "pool_idle_timeout_s": cfg.pool_idle_timeout_s,
            },
        }),
        Some("rust:wylde-ollama"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register the 10 actions on the process-wide registry. install()
    // must precede serve() so the registry is populated when the first
    // pipe client connects.
    wylde_ollama::service::install();

    // Write the action contract on disk for `wylde_check` and the
    // cross-language registry. Path resolves to
    // `data/contracts/actions/wylde-ollama.json` under WYLDE_ROOT.
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-ollama: action contract write failed: {e}");
    }

    // Best-effort liveness probe against the local Ollama daemon. A
    // warning is fine — Ollama may start lazily and the first chat
    // call will surface a clean ollama_unreachable error if it never
    // does.
    tokio::spawn(async {
        match wylde_ollama::upstream::client().health().await {
            Ok(()) => tracing::info!("wylde-ollama: upstream Ollama reachable"),
            Err(e) => tracing::warn!(
                "wylde-ollama: upstream Ollama unreachable at startup: {e} \
                 (will be retried on first call)"
            ),
        }
    });

    tracing::info!(
        "wylde-ollama: actions registered; opening pipe at \\\\.\\pipe\\wylde-ollama"
    );

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-ollama: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-ollama: ctrl-c received, shutting down");
        }
    }

    wylde_ollama::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-ollama: mark_stopped failed: {e}");
    }
    Ok(())
}
