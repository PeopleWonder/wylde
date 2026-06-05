//! `prompt/` — the workspace's contribution to a chat-turn prompt.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Prompt/`.
//!
//! This is the keystone of the redesign: workspaces are *config that
//! shapes prompt building*, and this module is where that happens. Given
//! a `workspace_id` (pulled off `chat.run_turn`) and the turn context,
//! it gathers the three workspace-scoped inputs —
//!
//! 1. persona override ([`super::persona`]),
//! 2. workspace-layer memory entries ([`super::memory`]),
//! 3. RAG snippets scoped to the workspace folder ([`super::rag`]) —
//!
//! and folds them into the system prompt slots. It slots in alongside
//! [`crate::turn::prompt::build_system_prompt`], which today builds only
//! the base-instruction + tool-catalog block (the memory-slot stack was
//! explicitly deferred — see that module's docs).
//!
//! ## Split
//!
//! * [`inject`] — [`WorkspaceContext`] (the gathered inputs) + the
//!   `inject` entrypoint the turn driver calls.

pub mod inject;

pub use inject::WorkspaceContext;
