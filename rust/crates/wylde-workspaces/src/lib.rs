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
//! **Slice 0a** stood up the bedrock: config, the service-wide error type,
//! the action registry, the pipe server wrapper, and a no-op `ping` verb.
//!
//! **Slice 0b (this slice)** relocated the workspace [`registry`], [`persona`],
//! and [`rag`] (incl. the graph-ingest pipeline in [`rag::indexer::graph_writer`])
//! from the harness, plus the thin infra they need — [`common`] fs/embed
//! helpers, the [`embeddings`] bridge, and the narrow [`graph`] Bolt write
//! client. The verb surface ([`api`]) is registered on the pipe by
//! [`action_dispatch`]. Workspace notes + conversations stay in the harness
//! until 0c; consumers are repointed off the harness compat shim in 0d.
//!
//! Public entry points:
//!   * [`action_dispatch::install`] — register the action surface. Idempotent.
//!   * [`action_dispatch::stop`] — drain background workers (none yet).
//!   * [`ipc::serve`] — bind the pipe and run the accept loop.

pub mod action_dispatch;
pub mod api;
pub mod common;
pub mod config;
pub mod embeddings;
pub mod error;
pub mod graph;
pub mod ipc;
pub mod persona;
pub mod rag;
pub mod registry;

#[cfg(test)]
mod test_support;

pub use action_dispatch::{install, reset_for_tests, stop};
pub use config::Config;
pub use error::{Result, WorkspacesError};
pub use persona::PersonaOverride;
pub use rag::WorkspaceRagScope;
pub use registry::{WorkspaceDefinition, WorkspaceState};
