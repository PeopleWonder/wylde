//! wylde-ext-webcrawler entry point.
//!
//! A standalone MCP-over-stdio server the `wylde-extension-bridge` spawns in
//! place of the Python `_shim` for the Webcrawler extension. Reads JSON-RPC
//! from stdin, writes responses to stdout, logs to stderr. Same greenfield
//! Rust shape as `wylde-ollama` / `wylde-treesitter`, but stdio-framed rather
//! than pipe-framed (the bridge owns the transport).
//!
//! See `docs/plans/legacy-extensions-rust-rewrite.md` (Slice 1).

use anyhow::Result;
use tracing_subscriber::EnvFilter;

const SERVICE_NAME: &str = "wylde-ext-webcrawler";

#[tokio::main]
async fn main() -> Result<()> {
    init_stderr_logging();
    tracing::info!("{SERVICE_NAME}: starting MCP stdio server (rust impl)");
    let result = wylde_ext_webcrawler::serve().await;
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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()  // MCP stdio server: stdout is the JSON-RPC channel, so logging must go to stderr, not configure_logging's stdout writer (wylde-check: logging-init-ok)
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}
