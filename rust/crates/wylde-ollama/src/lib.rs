//! Ollama inference proxy — single pipe surface for the local Ollama daemon.
//!
//! Phase 1 of the Rust migration. Mirrors `Core/harness/backend/ollama_client.py`
//! semantics behind ten pipe actions on `\\.\pipe\wylde-ollama`. Every
//! Ollama HTTP call that the harness makes today routes through here in
//! the strangler-fig phase, and direct HTTP from Python disappears in
//! the cleanup phase (master plan §1 Phase C).
//!
//! Public entry points:
//!   * [`service::install`] — register the `ollama.*` action surface +
//!     spawn the warm reqwest client. Idempotent.
//!   * [`service::stop`] — drain background workers.
//!   * [`service::reset_for_tests`] — clear singletons; for tests only.

pub mod actions;
pub mod config;
pub mod estimate;
pub mod lease;
pub mod service;
pub mod upstream;

pub use service::{install, reset_for_tests, stop};
