//! wylde-n8n service entry point.
//!
//! Boots the manifest, registers the action surface (8 actions), opens
//! the pipe at `\\.\pipe\wylde-n8n`, and serves until Ctrl-C. Same
//! shape as `wylde-ollama/main.rs` — the Wylde user's standing pattern.
//!
//! This process supervises the Wylde-side pipe surface ONLY. The n8n
//! daemon itself is external and user-managed (default
//! `http://127.0.0.1:5678`); an unreachable daemon degrades every call
//! to a structured error envelope, never a crash.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-n8n";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-n8n: starting (rust impl)");

    let cfg = wylde_n8n::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "optional",
        "N8N workflow service — single pipe surface for the external n8n \
         daemon (list/get/execute/create/edit/delete workflows + execution \
         status). Core works with or without it.",
        json!({
            "wylde_n8n": {
                "actions": [
                    "n8n.health",
                    "n8n.list_workflows",
                    "n8n.get_workflow",
                    "n8n.get_execution",
                    "n8n.execute_workflow",
                    "n8n.create_workflow",
                    "n8n.edit_workflow",
                    "n8n.delete_workflow",
                ],
                "upstream_url": cfg.auth.url.clone(),
                "auth_configured": cfg.auth.auth_ready(),
                // Data/template home per the registry convention — the
                // service folder keeps workflow templates only.
                "data_home": "N8N/workflow_templates",
            },
        }),
        Some("rust:wylde-n8n"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register the 8 actions on the process-wide registry. install()
    // must precede serve() so the registry is populated when the first
    // pipe client connects.
    wylde_n8n::service::install();

    // Write the action contract on disk for `wylde_check` and the
    // cross-language registry. Path resolves to
    // `data/contracts/actions/wylde-n8n.json` under WYLDE_ROOT.
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-n8n: action contract write failed: {e}");
    }

    // Best-effort liveness probe against the external n8n daemon. A
    // warning is fine — n8n is user-managed and may come up later; the
    // first call surfaces a clean error envelope if it never does.
    tokio::spawn(async {
        let health = wylde_n8n::actions::client().health().await;
        if health["reachable"].as_bool().unwrap_or(false) {
            tracing::info!("wylde-n8n: external n8n daemon reachable");
        } else {
            tracing::warn!(
                "wylde-n8n: external n8n daemon unreachable at startup \
                 (user-managed; calls degrade to structured errors until it's up)"
            );
        }
        if !health["auth_configured"].as_bool().unwrap_or(false) {
            tracing::warn!(
                "wylde-n8n: no credentials configured — set WYLDE_N8N_API_KEY \
                 or WYLDE_N8N_EMAIL+WYLDE_N8N_PASSWORD to enable workflow calls"
            );
        }
    });

    tracing::info!("wylde-n8n: actions registered; opening pipe at \\\\.\\pipe\\wylde-n8n");

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-n8n: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-n8n: ctrl-c received, shutting down");
        }
    }

    wylde_n8n::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-n8n: mark_stopped failed: {e}");
    }
    Ok(())
}
