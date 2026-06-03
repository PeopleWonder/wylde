//! wylde-ext-study entry point.
//!
//! A standalone MCP-over-stdio server the `wylde-extension-bridge` spawns in
//! place of the Python `_shim` for the Wylde_Study extension. Reads JSON-RPC
//! from stdin, writes responses to stdout, logs to stderr. Same stdio-framed
//! shape as `wylde-ext-webcrawler` (the bridge owns the transport).
//!
//! Unlike the Python Study handler — which reaches RAG/chat by importing
//! `Core.harness.*` libraries in-process — this binary reaches the harness
//! exclusively over the pipe via the S2a verbs (`rag.add_episodic`,
//! `rag.search`, `chat.complete`). See
//! `docs/plans/wylde-study-port-verification.md` (Slice S1).

use anyhow::Result;
use tracing_subscriber::EnvFilter;

const SERVICE_NAME: &str = "wylde-ext-study";

#[tokio::main]
async fn main() -> Result<()> {
    init_stderr_logging();
    tracing::info!("{SERVICE_NAME}: starting MCP stdio server (rust impl)");
    let result = wylde_ext_study::serve().await;
    if let Err(ref e) = result {
        tracing::error!("{SERVICE_NAME}: serve() exited with error: {e}");
    }
    result
}

/// stdout is reserved for JSON-RPC frames, so tracing MUST go to stderr —
/// mirroring the Python shim's stdout/stderr split. We can't reuse
/// `wylde_shared::logging::configure_logging` here because it installs a
/// stdout writer, which would corrupt the protocol stream.
fn init_stderr_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}
