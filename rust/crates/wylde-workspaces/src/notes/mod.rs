//! `notes/` — the per-workspace notes tier (the workspace-tier memory).
//!
//! **Conceptual path:** `Core/Workspaces/Notes/`.
//!
//! The **middle tier** of the 3-layer memory architecture (long-term /
//! **workspace** / short-term). Each workspace owns one bucket of curated
//! notes; at prompt-build time the highest-scoring entries are injected as a
//! workspace-memory slot, and the `workspaces.notes.*` verbs expose CRUD +
//! scoped search + the reflection proposal primitive.
//!
//! Relocated from the harness `workspaces::memory` (Slice 0c) — the slice
//! 0b move deliberately left this tier behind. Storage is byte-identical to
//! the harness original: `<data_dir>/workspaces/<workspace_id>/memory.jsonl`
//! (resolved through [`crate::registry::persistence`]), so existing notes are
//! picked up with no migration.
//!
//! Do NOT confuse this with [`crate::registry`]: registry stores workspace
//! *configs*, this stores *notes*.
//!
//! ## Split
//!
//! * [`entry`] — the [`WorkspaceMemoryEntry`] / [`NoteEntry`] type + the
//!   `memory.jsonl` IO + CRUD helpers.
//! * [`query`] — recency+relevance scoring for prompt injection + search.
//! * [`reflection`] — the (user-accept) note-proposal primitive.
//! * [`api`] — the `workspaces.notes.*` verb handlers.

pub mod api;
pub mod entry;
pub mod query;
pub mod reflection;

pub use entry::{NoteEntry, WorkspaceMemoryEntry};
pub use query::WorkspaceMemoryQuery;
