//! `memory` — compat re-export of the relocated workspace-notes tier.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Memory/`.
//!
//! Slice 0c moved this tier (the workspace-layer notes — entry type, JSONL
//! IO, recency+relevance scoring) into the `wylde-workspaces` service at
//! [`wylde_workspaces::notes`]. It's re-exported here so the existing
//! `crate::workspaces::memory::*` paths — chiefly the prompt builder
//! ([`super::prompt`], which gathers the workspace-memory slot in-process)
//! — keep resolving unchanged through the migration window. Slice 0d
//! repoints those readers at the service pipe and removes this shim.
//!
//! The verb surface (`workspaces.notes.*`) lives on the service pipe, with
//! harness compat-shim proxies in [`super::notes_api`].

pub use wylde_workspaces::notes::{
    entry, query, NoteEntry, WorkspaceMemoryEntry, WorkspaceMemoryQuery,
};
