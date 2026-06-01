//! device_gate service entry point.
//!
//! Rust equivalent of `python -m device_gate.run`. Writes the manifest,
//! starts a heartbeat, installs the `device_gate.*` action surface, and
//! serves the pipe until the host signals shutdown.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

// Service identity matches the Python `device_gate/run.py`:
//   manifest path → `data/manifests/wylde-device-gate.json`
//   pipe          → `\\.\pipe\wylde-device-gate`
const SERVICE_NAME: &str = "wylde-device-gate";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("device_gate: starting (rust impl)");

    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        Some(0),
        "auth",
        "Per-device pairing + permission tiers. Issues tokens that Gateway verifies on every external request.",
        json!({
            "dashboard": {
                "label": "device_gate",
                "icon": "shield",
                "color": "yellow",
            },
        }),
        Some("rust:wylde-device-gate"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    wylde_device_gate::pipe::install();
    tracing::info!(
        "device_gate: actions registered; opening pipe at \\\\.\\pipe\\wylde-device-gate"
    );

    let serve_fut = wylde_shared::ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("device_gate: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("device_gate: ctrl-c received, shutting down");
        }
    }

    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("device_gate: mark_stopped failed: {e}");
    }
    Ok(())
}
