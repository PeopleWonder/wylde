//! `wylde-workspaces` — the workspace-scoped service of the Thought Bubble
//! System.
//!
//! This is the new top-level service that owns everything workspace-scoped:
//! the workspace registry, per-workspace notes, workspace conversations,
//! the anchor/world-model layer, the code-graph projection, ingest, and the
//! file watcher. Consumers (the harness chat-turn driver, the GUI panels)
//! reach it over `\\.\pipe\wylde-workspaces` through the shared
//! `wylde-workspaces-client` crate — never by hand-rolling the pipe.
//!
//! **Slice 0a (this scaffold)** stands up only the bedrock: config, the
//! service-wide error type, the action registry with a single no-op `ping`
//! verb, and the pipe server wrapper. No registry / notes / conversations /
//! anchors / graph / ingest / migration yet — those land in 0b → A per the
//! build-order doc on the Nextcloud Wylde collective.
//!
//! Public entry points:
//!   * [`action_dispatch::install`] — register the action surface. Idempotent.
//!   * [`action_dispatch::stop`] — drain background workers (none yet).
//!   * [`ipc::serve`] — bind the pipe and run the accept loop.

pub mod action_dispatch;
pub mod config;
pub mod error;
pub mod ipc;

pub use action_dispatch::{install, reset_for_tests, stop};
pub use config::Config;
pub use error::{Result, WorkspacesError};
