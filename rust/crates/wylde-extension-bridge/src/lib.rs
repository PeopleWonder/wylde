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
/// active. Gates the Slice-5a claimed-tool partition: when off, the new
/// `resources[]` field is still parsed and exposed via `ext.resources.list`
/// (harmless), but `aggregate_tools` does **not** subtract claimed tools,
/// so named-tool behaviour is unchanged. The harness reads the same flag
/// to decide whether to populate its verb overlay — one variable flips
/// both sides together. Accepts `1`/`true`/`yes`/`on` (case-insensitive).
pub fn verb_mode_active() -> bool {
    std::env::var("WYLDE_HARNESS_VERB_TOOLS")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}
