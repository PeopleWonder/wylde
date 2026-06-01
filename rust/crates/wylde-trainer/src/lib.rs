//! Trainer service — pipe surface for the Caption sub-service (Florence-2).
//!
//! Phase 3 of the Rust migration. The existing Python `Trainer/Caption/`
//! module is in-process today; this crate fronts the same captioner over
//! `\\.\pipe\wylde-trainer`, forwarding every inference action to a
//! sibling Python pipe service at `\\.\pipe\wylde-trainer-worker`.
//!
//! The worker (`Trainer/Caption/rust_worker.py`) is supervised by the
//! lifecycle daemon, NOT by this crate — the `no_external_process_spawn_rust`
//! lint rule pins `Command::new` to `wylde-lifecycle`. This crate is a
//! thin pipe-to-pipe forwarder. See [`worker_client`] for the call site.
//!
//! Subprocess-routed Python is chosen over a pure-Rust `ort` path
//! because Microsoft's Florence-2 ships `trust_remote_code=True` custom
//! modeling plus a task-aware `processor.post_process_generation` step
//! that has no clean ONNX export. The Rust pipe surface is the Phase 3
//! goal; this crate delivers it without re-implementing the inference
//! engine.
//!
//! Public entry points:
//!   * [`service::install`] — register the `caption.*` action surface.
//!     Idempotent.
//!   * [`service::stop`] — no-op (no per-process workers to drain).
//!   * [`service::reset_for_tests`] — clear singletons; for tests only.

pub mod actions;
pub mod config;
pub mod service;
pub mod worker_client;

pub use service::{install, reset_for_tests, stop};
