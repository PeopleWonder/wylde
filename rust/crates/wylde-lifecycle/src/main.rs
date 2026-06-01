//! `wylde-lifecycle` binary entry point.
//!
//! Wires the tokio runtime to [`wylde_lifecycle::daemon::serve_forever`].
//! The Python equivalent is `python -m Core.Lifecycle.daemon`; the
//! launcher script picks Python vs Rust via `WYLDE_LIFECYCLE_IMPL`.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match wylde_lifecycle::daemon::serve_forever().await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("wylde-lifecycle: fatal: {e:#}");
            ExitCode::from(1)
        }
    }
}
