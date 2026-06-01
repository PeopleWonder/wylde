//! Workspaces panel — gpui-era port of
//! `Core/GUI/src/pages/Workspaces.svelte`.
//!
//! Scope of slice 3:
//!   - List the workspaces the harness reports via
//!     `rag.workspaces.list` (MRU order, single path per workspace
//!     under the harness model — see `wylde_rag_workspaces` memory).
//!   - Show the currently-active workspace inline at the top.
//!   - Add a workspace via the native folder picker (`rfd`) →
//!     `rag.workspaces.activate`.
//!   - Re-index (`rag.workspaces.reindex`) and remove
//!     (`rag.workspaces.delete`) per-row.
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

pub mod ipc;
pub mod workspaces_panel;

pub use workspaces_panel::WorkspacesPanel;
