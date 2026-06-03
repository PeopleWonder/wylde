//! wylde-extension-bridge — Rust MCP-server host for Wylde extensions.
//!
//! Replaces the Python `Extensions.extension_bridge.*` importlib-driven
//! dispatcher. Extensions are now MCP servers (separate processes); this
//! crate spawns each one, performs the MCP `initialize` handshake,
//! supervises it, and exposes a 9-action surface on
//! `\\.\pipe\wylde-extension-bridge`.
//!
//! Public entry points:
//!   * [`service::install`] — register the 9 + 1 alias action surface.
//!   * [`service::stop`]   — drain background workers + reap children.
//!   * [`host::Host`]      — supervisor for a set of MCP-server children.

pub mod config;
pub mod discovery;
pub mod host;
pub mod manifest;
pub mod mcp;
pub mod service;
pub mod version;

pub mod actions;

pub use service::{install, reset_for_tests, stop};

/// True when the verb-tool cutover flag (`WYLDE_HARNESS_VERB_TOOLS`) is
/// active. Gates the Slice-5a claimed-tool partition: when active,
/// `aggregate_tools` subtracts tools claimed by a `resources[]`
/// declaration (so a claimed tool is reachable only through the verb
/// surface, never double-advertised); when off, the `resources[]` field
/// is still parsed and exposed via `ext.resources.list` but no claimed
/// tools are subtracted, so named-tool behaviour is unchanged. The
/// harness reads the same flag to decide whether to populate its verb
/// overlay — one variable flips both sides together.
///
/// **Slice 6 cutover (2026-06-03):** the default flipped from off to
/// **on**, in lockstep with the harness twin
/// (`wylde-harness::tooling::resource::verb_mode_active`). Accepts
/// `1`/`true`/`yes`/`on` (case-insensitive); any other value — or the
/// explicit opt-out — falls back to the deprecated named-tool partition.
pub fn verb_mode_active() -> bool {
    match std::env::var("WYLDE_HARNESS_VERB_TOOLS") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        // Slice 6: default on.
        Err(_) => true,
    }
}
