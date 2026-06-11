//! N8N service — single pipe surface for the local n8n workflow engine.
//!
//! Taxonomy reorg TX S3 (2026-06-11). N8N is a SERVICE in Aaron's
//! architecture taxonomy — a sibling full-tier suite next to
//! wylde-workspaces / wylde-voice — and Core must work with or without
//! it. This crate is the Rust port of the dead-since-the-Python-cutover
//! `N8N/client.py` REST client, fronted by eight `n8n.*` actions on
//! `\\.\pipe\wylde-n8n`. The n8n daemon itself stays an **external,
//! user-managed runtime** (default `http://127.0.0.1:5678`); this
//! service only owns the Wylde-side surface, exactly the way
//! wylde-ollama fronts the external Ollama daemon.
//!
//! Direct HTTP to the n8n URL is sanctioned (Wylde Design Principle 9 —
//! services we don't own may use HTTP); everything else reaches this
//! crate over the pipe.
//!
//! Public entry points:
//!   * [`service::install`] — register the `n8n.*` action surface.
//!     Idempotent.
//!   * [`service::stop`] — drain background workers (none today).
//!   * [`service::reset_for_tests`] — clear registrations; tests only.

pub mod actions;
pub mod client;
pub mod config;
pub mod service;

pub use service::{install, reset_for_tests, stop};
