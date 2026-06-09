//! `prompt/` — the workspace's contribution to a chat-turn prompt.
//!
//! **Conceptual path:** `Core/Workspaces/Prompt/`.
//!
//! This is the keystone of the redesign: workspaces are *config that
//! shapes prompt building*, and this module is where that happens. Given
//! a `workspace_id` and the turn's user message, it gathers the three
//! workspace-scoped inputs —
//!
//! 1. persona override ([`crate::persona`]),
//! 2. workspace-layer notes ([`crate::notes`]),
//! 3. RAG snippets scoped to the workspace folder ([`crate::rag`]) —
//!
//! and renders them into the system-prompt slot text the harness chat
//! turn driver appends to its base prompt.
//!
//! ## Relocation (Slice 0d)
//!
//! Through Slices 0b/0c this gather ran **in-process inside the harness**
//! (the harness's old `workspaces::prompt` module). Slice 0d moves it here,
//! behind the `workspaces.gather_prompt` verb, so the harness no longer
//! carries any workspace code — it fetches the rendered slots over the pipe
//! via the `wylde-workspaces-client` crate and degrades gracefully when the
//! service is unreachable.
//!
//! ## Split
//!
//! * [`inject`] — [`WorkspaceContext`] (the gathered inputs) + the
//!   `gather` / `render_slots` entry points the [`crate::api`] verb wraps.

pub mod inject;

pub use inject::{gather, render_slots, WorkspaceContext};
