//! wylde-trainer service entry point.
//!
//! Boots the manifest, registers the action surface (5 actions), opens
//! the pipe at `\\.\pipe\wylde-trainer`, and serves until Ctrl-C. Same
//! shape as `wylde-ollama/main.rs` — the Wylde user's standing pattern.
//!
//! Florence-2 inference runs in a sibling Python pipe service
//! `wylde-trainer-worker` (`Trainer/Caption/rust_worker.py`) supervised
//! by the lifecycle daemon. This crate forwards every inference action
//! to that pipe via `wylde_shared::ipc::send_action`. Process spawning
//! lives in `wylde-lifecycle` by policy (the `no_external_process_spawn_rust`
//! lint pins `Command::new` there).

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-trainer";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-trainer: starting (rust impl)");

    let cfg = wylde_trainer::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "standard",
        "Trainer service — Florence-2 captioning pipe surface for the Caption sub-service.",
        json!({
            "wylde_trainer": {
                "actions": wylde_trainer::service::all_actions(),
                "backend": cfg.backend.clone(),
                "florence_variant": cfg.florence_variant.clone(),
                "default_detail": cfg.default_detail.clone(),
                "worker_pipe": wylde_trainer::worker_client::WORKER_SERVICE,
            },
        }),
        Some("rust:wylde-trainer"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register actions on the process-wide registry. install() must
    // precede serve() so the registry is populated when the first pipe
    // client connects.
    wylde_trainer::service::install();

    // Write the action contract on disk for `wylde_check` and the
    // cross-language registry. Path resolves to
    // `data/contracts/actions/wylde-trainer.json` under WYLDE_ROOT.
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-trainer: action contract write failed: {e}");
    }

    tracing::info!(
        "wylde-trainer: actions registered; opening pipe at \\\\.\\pipe\\wylde-trainer"
    );

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-trainer: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-trainer: ctrl-c received, shutting down");
        }
    }

    wylde_trainer::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-trainer: mark_stopped failed: {e}");
    }
    Ok(())
}
