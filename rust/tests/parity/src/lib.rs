//! Cross-language parity harness for the Wylde Python -> Rust cutover.
//!
//! Originally this crate gated every service's cutover (gateway, vram
//! broker, device gate, lifecycle). Most of those Python implementations
//! have since been deleted — the strangler-fig is done for them — and their
//! parity tests were retired with their targets (see this crate's README,
//! "Retired suites"). Two suites remain:
//!
//! - **`lifecycle`** — the Python lifecycle daemon (`Core/Lifecycle/`) is
//!   still load-bearing and not yet cut over, so its no-spawn control
//!   surface still has a live Python half to diff the Rust port against.
//! - **`wylde-ollama`** — greenfield Rust with no Python counterpart; a
//!   record/replay smoke against a live Ollama daemon.
//!
//! ## Layout
//!
//! - [`paths`] — locate the repo root, the `.venv` interpreter, the release
//!   binaries.
//! - [`proc`] — spawn a service implementation as a child process and kill
//!   it on drop.
//! - [`diff`] — normalize volatile fields (timestamps, ids, pids, hardware
//!   readings) out of a JSON value, then assert structural parity.
//! - [`pipe`] — drive a named-pipe service via `wylde_shared::ipc` and
//!   capture the reply envelope (lifecycle / ollama parity).
//!
//! The test targets under `tests/` are gated behind the `parity` feature so
//! `cargo test` without `--features parity` does nothing — see the README.

pub mod diff;
pub mod paths;
pub mod pipe;
pub mod proc;
