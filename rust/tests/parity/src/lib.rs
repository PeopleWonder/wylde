//! Cross-language parity harness for the Wylde Python -> Rust cutover.
//!
//! The four services (`wylde-gateway`, `wylde-vram-broker`,
//! `wylde-device-gate`, `wylde-lifecycle`) each have a Python implementation
//! and a Rust port. Before a service's `WYLDE_*_IMPL` default can flip to
//! `rust`, the two implementations must answer identical requests
//! identically. This crate fires the same request at both and diffs the
//! response.
//!
//! ## Layout
//!
//! - [`paths`] — locate the repo root, the `.venv` interpreter, the release
//!   binaries.
//! - [`proc`] — spawn a service implementation as a child process and kill
//!   it on drop.
//! - [`diff`] — normalize volatile fields (timestamps, ids, pids, hardware
//!   readings) out of a JSON value, then assert structural parity.
//! - [`http`] — fire an HTTP request at a running gateway and capture the
//!   response (Gateway parity).
//! - [`pipe`] — drive a named-pipe service via `wylde_shared::ipc` and
//!   capture the reply envelope (VRAM broker / device gate parity).
//!
//! The test targets under `tests/` are gated behind the `parity` feature so
//! `cargo test` without `--features parity` does nothing — see the README.

pub mod diff;
pub mod http;
pub mod paths;
pub mod pipe;
pub mod proc;
