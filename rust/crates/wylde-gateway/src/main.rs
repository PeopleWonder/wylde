//! Wylde Gateway service entry point.
//!
//! Rust equivalent of `python -m Gateway.run`. The real entry sequence
//! lives in [`wylde_gateway::run::main`] so the unit tests can construct
//! a Gateway without spawning the full process.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    wylde_gateway::run::main().await
}
