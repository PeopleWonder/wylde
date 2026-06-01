//! Service-wide startup sequence — Rust port of `Gateway/run.py`.
//!
//! Wave-1 startup order (matches Python's documented sequence, with
//! out-of-scope steps deferred per `docs/r3_gateway_deferred.md`):
//!
//!   1. `configure_logging`
//!   2. load settings
//!   3. `ManifestWriter::write` + `start_heartbeat` (every 60s)
//!   4. register pipe actions (`pipe::install`)
//!   5. bind axum on `host:port` and serve until ctrl-c / SIGTERM
//!   6. serve named-pipe transport concurrently
//!   7. `mark_stopped` on the way out
//!
//! The Lifecycle daemon spawns this binary when
//! `WYLDE_WYLDE_GATEWAY_IMPL=rust`; Python's `Gateway.run` is the other
//! arm of the strangler-fig.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::Level;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

use crate::app::build_router;
use crate::settings::get_settings;
use crate::SERVICE_NAME;

/// Manifest description — matches `Gateway/run.py::write_manifest(description=...)`.
const MANIFEST_DESCRIPTION: &str = "Unified ingress/egress for Wylde. Axum on 127.0.0.1:8005. \
     All external HTTP traffic in or out of the mesh flows through here so \
     allowlist, kill switch, auth, rate limiting, and audit logging are \
     enforced in one place. Hosts the /extensions/<name>/<endpoint> routes \
     for browser extensions.";

/// Real entry point. Called by `main.rs` after the tokio runtime starts.
pub async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("gateway: starting (rust impl, R3 wave 1)");

    let settings = get_settings();
    let bind_addr: SocketAddr = format!("{}:{}", settings.host, settings.port)
        .parse()
        .with_context(|| format!("invalid host:port {}:{}", settings.host, settings.port))?;

    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        Some(settings.port),
        "gateway",
        MANIFEST_DESCRIPTION,
        json!({
            "dashboard": {"label": "Gateway", "icon": "globe", "color": "teal"},
        }),
        Some("rust:wylde-gateway"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Wave 2e startup: apply the kill-switch env bootstrap and walk every
    // component manifest to populate the egress allowlist. Both are
    // best-effort — failures log and continue (matches Python).
    crate::egress::kill_switch::apply_env_bootstrap();
    crate::egress::destinations::reload(None);

    crate::pipe::install();
    tracing::info!("pipe: actions registered, opening pipe at \\\\.\\pipe\\wylde-gateway");

    let app = build_router(settings.clone());
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("gateway: bind {bind_addr}"))?;
    tracing::info!(
        "gateway listening on {} (workers={})",
        bind_addr,
        settings.workers
    );

    // HTTP and pipe surfaces run concurrently; either ctrl-c or a server
    // error tears the whole thing down. The pipe future shares the
    // process for its lifetime — `mark_stopped()` after this `select!`
    // is the only observable shutdown signal callers need.
    let http_fut = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    let pipe_fut = wylde_shared::ipc::serve(SERVICE_NAME, None);

    tokio::select! {
        result = http_fut => {
            if let Err(e) = result {
                tracing::error!("gateway: HTTP serve exited with error: {e}");
            } else {
                tracing::info!("gateway: HTTP serve completed");
            }
        }
        result = pipe_fut => {
            if let Err(e) = result {
                tracing::error!("gateway: pipe serve exited with error: {e}");
            }
        }
    }

    crate::middleware::audit_log::reset_audit_writers();

    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("gateway: mark_stopped failed: {e}");
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("gateway: ctrl-c received, shutting down"),
        _ = terminate => tracing::info!("gateway: SIGTERM received, shutting down"),
    }
}
