//! Lifecycle daemon — Rust port of `Core/Lifecycle/`.
//!
//! Long-running supervisor that spawns Wylde's tier=core services
//! (Memgraph, Voice, VRAM broker, device_gate, Gateway, memory
//! scheduler), serves the `\\.\pipe\wylde-lifecycle` action surface
//! so the GUI can drive start/stop/wake/list/health, and runs the
//! periodic orphan-detection sweep that flips dead-pid manifests to
//! `dead-orphan`.
//!
//! Module layout mirrors `Core/Lifecycle/`'s per-module split — the Wylde user's
//! standing instruction is one Rust file per Python module:
//!
//! * [`daemon`] — the [`daemon::serve_forever`] entry point that walks
//!   spawn → register actions → serve pipe → block phases.
//! * [`control`] — pipe action handlers (`service.shutdown_all`, etc.).
//! * [`state`] — daemon-managed subprocess + scheduler handles, the
//!   stop-event API, and the unified [`state::stop_all_daemon_managed`]
//!   teardown that both the SIGINT handler and the pipe action go
//!   through.
//!   * [`state::manifest`] — Core's runtime manifest (`core.json`) +
//!     heartbeat helpers.
//!   * [`state::orphan_sweep`] — the 60s sweep that walks
//!     `data/manifests/*.json` and marks dead pids.
//!   * [`state::services`] — six `start_<service>` / `stop_<service>`
//!     pairs plus the env-var dispatch that picks Python vs Rust
//!     implementations during the strangler-fig migration.
//!
//! Strangler-fig (launch-time): the launcher script picks Python or
//! Rust via `WYLDE_LIFECYCLE_IMPL`. Both daemons read the same
//! manifest schema (verified by `wylde-shared::manifest`'s parity
//! tests), so the service health view stays consistent regardless of
//! which is running.

pub mod control;
pub mod daemon;
pub mod registry;
pub mod state;
