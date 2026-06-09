//! Workspaces panel — gpui-era port of
//! `Core/GUI/src/pages/Workspaces.svelte`.
//!
//! Scope (config-file-backed workspaces redesign, PR #12 + RAG indexer
//! PR #18 — `workspaces.*` verb surface):
//!   - List the workspaces the harness reports via
//!     `workspaces.list_mru` (static MRU-5, single folder per workspace).
//!   - Show the currently-active workspace inline at the top.
//!   - Add a workspace via the native folder picker (`rfd`) →
//!     `workspaces.create`; "Switch" → `workspaces.set_active`.
//!   - Re-index (`workspaces.reindex`) and remove
//!     (`workspaces.delete`) per-row.
//!
//! Out of scope for this slice (next slices):
//!   - Per-workspace persona editor.
//!   - The "stale workspaces" cleanup banner — the harness MRU caps
//!     at 5 and auto-evicts (see Svelte workspaces.js), so there's no
//!     separate stale surface in the gpui edition.
//!   - The conversation-binding flow (the Svelte page can bind a
//!     workspace to a conversation; that arrives with the Chat port).
//!
//! The Svelte original stays the source of truth during the alpha; we
//! don't touch it.  Cutover deletes `src-tauri/` + `src/` together.

//! ## Tabs (Thought Bubble System, Phase 3)
//!
//! The panel now hosts a minimal tab system ([`tabs`]): the original
//! Registry plus a new Graph tab (Slice C-scaffold) that mounts the visual
//! code-graph view ([`graph::GraphView`]). Vocabulary / Conversations /
//! Settings tabs are wired by their own later slices.

pub mod graph;
pub mod ipc;
pub mod tabs;
pub mod workspaces_panel;

pub use workspaces_panel::WorkspacesPanel;
