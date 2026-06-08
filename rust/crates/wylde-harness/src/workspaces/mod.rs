//! `workspaces/` — the harness's workspace surface during the Thought
//! Bubble System service-extraction window.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/` in Aaron's Python-style
//! mental model.
//!
//! ## What lives here now (Slice 0b)
//!
//! The workspace **registry**, **persona**, and **rag** (incl. the
//! graph-ingest pipeline) relocated to the new top-level `wylde-workspaces`
//! service crate. What remains in the harness:
//!
//! * [`api`] — a **thin compat-shim proxy**. The harness pipe still answers
//!   the `workspaces.*` verbs (consumers are pinned to it until Slice 0d),
//!   but each handler now forwards to the running `wylde-workspaces` service
//!   over its pipe via the `wylde-workspaces-client` crate, falling back to
//!   in-process execution (through the `wylde-workspaces` lib) when the
//!   service isn't reachable. Single source of truth = the new crate.
//! * [`memory`] — the per-workspace memory-entries tier (the workspace-tier
//!   notes). Still harness-owned; relocates in Slice 0c.
//! * [`prompt`] — assembles the workspace's contribution to a chat turn's
//!   system prompt from persona + memory + RAG. Calls the relocated
//!   registry / persona / rag in-process via the `wylde_workspaces` lib;
//!   repoints to the service pipe in Slice 0d.
//!
//! The registry / persona / rag / ingest types are re-exported from
//! [`wylde_workspaces`] so existing harness call sites keep their paths
//! through the migration window.

pub mod api;
pub mod memory;
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
