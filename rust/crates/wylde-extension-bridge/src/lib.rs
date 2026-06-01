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
