//! VRAM broker service entry point.
//!
//! Rust equivalent of `python -m Core.resource_monitor.run`. Writes the
//! service manifest, starts a heartbeat, installs the action surface, and
//! serves the pipe until the host signals shutdown.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

// Service identity matches the Python `Core/resource_monitor/run.py`: the
// on-disk manifest is `data/manifests/vram-broker.json`, the pipe is
// `\\.\pipe\wylde-vram-broker` (the `wylde-` prefix is added by
// `pipe_name` / `ManifestWriter::write` automatically).
const SERVICE_NAME: &str = "vram-broker";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("vram_broker: starting (rust impl)");

    // The broker writes its own manifest — daemon does not. Contributes
    // shape mirrors `Core/resource_monitor/run.py::write_manifest` so the
    // dashboard's per-broker diagnostics surface keeps the same keys
    // regardless of which impl is up. (Rust serves these endpoints over
    // pipe actions named `vram.<verb>`; the path strings are kept for
    // dashboard back-compat and as documentation of the surface.)
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "core",
        "GPU VRAM lease broker — priority-based admission control across all services",
        json!({
            "vram_broker": {
                "state_path": "/vram/state",
                "leases_path": "/vram/leases",
                "reserve_path": "/vram/reserve",
                "release_path": "/vram/release",
                "evict_path": "/vram/evict",
                "state_file": "data/state/vram-broker.json",
                "actions": [
                    "vram.reserve",
                    "vram.release",
                    "vram.heartbeat",
                    "vram.state",
                    "vram.leases",
                    "vram.cache",
                    "vram.evict",
                    "system.inventory",
                ],
            },
        }),
        Some("rust:wylde-vram-broker"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register actions and start background tasks. install() must come
    // before serve() so the action registry is populated when the first
    // pipe client connects.
    wylde_vram_broker::service::install(true);
    tracing::info!(
        "vram_broker: actions registered; opening pipe at \\\\.\\pipe\\wylde-vram-broker"
    );

    // Graceful shutdown: trigger stop on CTRL-C / CTRL-BREAK. The serve()
    // future races against ctrl_c so we can mark the manifest stopped
    // before exit.
    let serve_fut = wylde_shared::ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("vram_broker: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("vram_broker: ctrl-c received, shutting down");
        }
    }

    wylde_vram_broker::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("vram_broker: mark_stopped failed: {e}");
    }
    Ok(())
}
