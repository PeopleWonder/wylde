//! `workspaces/` — the harness's workspace surface during the Thought
//! Bubble System service-extraction window.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/` in Aaron's Python-style
//! mental model.
//!
//! ## What lives here now (Slice 0c)
//!
//! The workspace **registry**, **persona**, **rag** (Slice 0b), and now the
//! **notes** tier + **workspace-scoped conversations** (Slice 0c) all live in
//! the top-level `wylde-workspaces` service crate. What remains in the
//! harness:
//!
//! * [`api`] — a **thin compat-shim proxy** for `workspaces.*`. Each handler
//!   forwards to the running `wylde-workspaces` service over its pipe via the
//!   `wylde-workspaces-client` crate, falling back to in-process execution
//!   (through the `wylde-workspaces` lib) when the service isn't reachable.
//!   Single source of truth = the new crate.
//! * [`notes_api`] — the same compat-shim proxy for the Slice 0c
//!   `workspaces.notes.*` verbs.
//! * [`conversations_api`] — the same proxy for the Slice 0c
//!   `workspaces.conversations.*` verbs (workspace-scoped conversations;
//!   **standalone** conversations stay in [`crate::memory::conversations`]).
//! * [`memory`] — a compat **re-export** of the relocated notes tier
//!   ([`wylde_workspaces::notes`]) so the in-process prompt builder's
//!   `crate::workspaces::memory::*` paths keep resolving until Slice 0d.
//! * [`prompt`] — assembles the workspace's contribution to a chat turn's
//!   system prompt from persona + notes + RAG, in-process via the
//!   `wylde_workspaces` lib; repoints to the service pipe in Slice 0d.
//!
//! The registry / persona / rag / notes types are re-exported from
//! [`wylde_workspaces`] so existing harness call sites keep their paths
//! through the migration window.

pub mod api;
pub mod conversations_api;
pub mod memory;
pub mod notes_api;
pub mod prompt;

#[cfg(test)]
mod test_support;

// ── Public surface ───────────────────────────────────────────────────────
//
// Workspace-config types now live in the `wylde-workspaces` crate; re-export
// them here so harness consumers (and any `crate::workspaces::*` paths that
// predate the move) resolve unchanged until Slice 0d repoints them.

pub use memory::{WorkspaceMemoryEntry, WorkspaceMemoryQuery};
pub use prompt::WorkspaceContext;
pub use wylde_workspaces::persona::PersonaOverride;
pub use wylde_workspaces::rag::WorkspaceRagScope;
pub use wylde_workspaces::registry::{WorkspaceDefinition, WorkspaceState};
