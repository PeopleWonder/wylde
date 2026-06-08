//! `conversations/` — workspace-scoped conversation storage + verbs.
//!
//! **Conceptual path:** `Core/Workspaces/Conversations/`.
//!
//! Slice 0c split the conversation tier by scope. Conversations bound to a
//! workspace (`workspace_id != None`) live here, one file per conversation
//! under `<data_dir>/workspaces/<workspace_id>/conversations/`; **standalone**
//! conversations (`workspace_id == None`) stay in the harness flat store and
//! are never touched by this service (plan §2 / §3 — strict scope, no leak
//! across workspaces).
//!
//! ## Split
//!
//! * [`store`] — per-workspace `Value`-based document IO (byte-identical to
//!   the harness flat-store shape; the migration relocates existing
//!   workspace-tagged docs into this layout).
//! * [`api`] — the `workspaces.conversations.*` lifecycle verbs.

pub mod api;
pub mod store;
