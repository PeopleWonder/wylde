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
//! **Slice 0b** relocated the workspace [`registry`], [`persona`], and
//! [`rag`] (incl. the graph-ingest pipeline in [`rag::indexer::graph_writer`])
//! from the harness, plus the thin infra they need — [`common`] fs/embed
//! helpers, the [`embeddings`] bridge, and the narrow [`graph`] Bolt write
//! client. The verb surface ([`api`]) is registered on the pipe by
//! [`action_dispatch`].
//!
//! **Slice 0c (this slice)** relocated the **workspace notes** tier
//! ([`notes`], the `workspaces.notes.*` verbs) and **workspace-scoped
//! conversations** ([`conversations`], the `workspaces.conversations.*`
//! verbs) — the memory-tier split. Standalone conversations stay
//! harness-owned. The [`migration`] module moves pre-split workspace
//! conversations into the new per-workspace layout on first service startup
//! (idempotent, marker-gated). Consumers are repointed off the harness compat
//! shim in 0d.
//!
//! Public entry points:
//!   * [`action_dispatch::install`] — register the action surface. Idempotent.
//!   * [`action_dispatch::stop`] — drain background workers (none yet).
//!   * [`migration::run_pending`] — idempotent first-startup data migration.
//!   * [`ipc::serve`] — bind the pipe and run the accept loop.

pub mod action_dispatch;
pub mod anchors;
pub mod api;
pub mod common;
pub mod config;
pub mod conversations;
pub mod embeddings;
pub mod error;
pub mod graph;
pub mod ipc;
pub mod migration;
pub mod notes;
pub mod persona;
pub mod prompt;
pub mod rag;
pub mod registry;
pub mod watcher;

#[cfg(test)]
mod test_support;

pub use action_dispatch::{install, reset_for_tests, stop};
pub use config::Config;
pub use error::{Result, WorkspacesError};
pub use notes::{NoteEntry, WorkspaceMemoryEntry, WorkspaceMemoryQuery};
pub use persona::PersonaOverride;
pub use rag::WorkspaceRagScope;
pub use registry::{WorkspaceDefinition, WorkspaceState};
