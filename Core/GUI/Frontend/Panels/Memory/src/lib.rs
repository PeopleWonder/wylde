//! Memory panel — gpui-era surface over the three-layer memory model.
//!
//! Each layer maps to a section in the View:
//!
//!   * **Long-term** — curated, importance-gated records shared across
//!     every conversation.  Pulled via `memory.long_term.list`; search
//!     uses `memory.long_term.search`.
//!   * **Workspace** — the recent workspaces (MRU-5).  Sourced from
//!     `workspaces.list_mru` (config-file-backed redesign, PR #12 —
//!     the retired `memory.workspaces.*` surface returned `no_action`).
//!   * **Short-term** — the rolling per-conversation buffer.  The Rust
//!     harness does not yet serve `memory.short_term.*` (see the §9
//!     strangler-fig deferred list); we surface the layer as a
//!     documented stub so the user sees where it sits without the
//!     panel pretending the data is there.
//!
//! Slice 5 scope:
//!   - Three sections render side-by-side in a column.
//!   - Long-term section ships search + filter + click-to-expand.
//!   - Workspace section lists the recent workspaces with personas.
//!   - Short-term section renders a placeholder strip until the
//!     `memory.short_term.*` port lands (tracked in the Phase 9
//!     punchlist).
//!
//! The Svelte original (`Core/GUI/src/components/MemorySettings.svelte`)
//! lives in the Settings tab and is intentionally narrower — it owns
//! delete + persona-write surfaces.  This panel is read-mostly; writes
//! land in a follow-on slice so the gpui port doesn't ship destructive
//! actions ahead of the panel-router's confirm dialogs.

pub mod ipc;
pub mod memory_panel;

pub use memory_panel::MemoryPanel;
