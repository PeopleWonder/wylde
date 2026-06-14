//! `wylde-lsp` — the optional rust-analyzer LSP host (IDE S8, OQ-1).
//!
//! Supervises a single `rust-analyzer` child and exposes diagnostics /
//! completions / hover over `lsp.*` pipe verbs. **Taxonomy-clean and
//! OPTIONAL:** the code editor works without it (plain text + tree-sitter
//! highlighting + the workspaces graph for go-to-def). When rust-analyzer is
//! absent the service stays up and reports `available:false` so the editor
//! degrades gracefully — never a hard dependency.
//!
//! - [`jsonrpc`] — LSP wire framing (pure, tested).
//! - [`client`] — the actor that owns the rust-analyzer child + LSP state.
//! - [`service`] — the `lsp.*` verb surface registered on the pipe.

pub mod client;
pub mod config;
pub mod jsonrpc;
pub mod service;

pub use config::Config;
pub use service::{install, reset_for_tests};
