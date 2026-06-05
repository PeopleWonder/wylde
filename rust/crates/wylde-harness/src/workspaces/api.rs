//! `api.rs` — the minimal IPC verb surface for workspaces.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/api`.
//!
//! ## Design stance: thin, write-mostly
//!
//! Because workspaces are config files, **reads do not need verbs** —
//! the GUI's Workspaces panel reads `workspaces.json` directly. The
//! harness owns *writes* (so there's a single writer + validation) and
//! the *active-selection* pointer the turn driver consumes. This is the
//! deliberate opposite of the retired `rag.workspaces.*` surface, which
//! exposed a full CRUD API.
//!
//! Proposed verbs (final set is an open question — see the design doc):
//!
//! * `workspaces.set_active` — set the active workspace + bump MRU. The
//!   one verb the Chat panel needs.
//! * `workspaces.create` — register a folder as a workspace.
//! * `workspaces.update` — rename / toggle `persona_enabled` /
//!   `rag_enabled`.
//! * `workspaces.delete` — remove a workspace + its `<workspace_id>/`
//!   data dir.
//! * `workspaces.set_persona` — write `persona.md`.
//!
//! Each handler returns a `wylde_shared::ipc::Reply`, matching the
//! existing action handlers (e.g. [`crate::memory::workspaces::actions`]).
//! Registration on the pipe lands at implementation time via
//! `crate::pipe`; this scaffold registers nothing.
//!
//! Scaffold only — no logic yet.

use serde_json::Value;
use wylde_shared::ipc::Reply;

/// `workspaces.set_active` — set the active workspace and move it to the
/// head of the MRU list. Payload: `{ "workspace_id": string }`.
pub async fn handle_set_active(_payload: Value) -> Reply {
    todo!("workspaces redesign: handle_set_active")
}

/// `workspaces.create` — register a folder as a workspace.
/// Payload: `{ "folder": string, "name"?: string }`.
pub async fn handle_create(_payload: Value) -> Reply {
    todo!("workspaces redesign: handle_create")
}

/// `workspaces.update` — rename / toggle feature flags.
/// Payload: `{ "workspace_id": string, "name"?: string,
/// "persona_enabled"?: bool, "rag_enabled"?: bool }`.
pub async fn handle_update(_payload: Value) -> Reply {
    todo!("workspaces redesign: handle_update")
}

/// `workspaces.delete` — remove a workspace + its data dir.
/// Payload: `{ "workspace_id": string }`.
pub async fn handle_delete(_payload: Value) -> Reply {
    todo!("workspaces redesign: handle_delete")
}

/// `workspaces.set_persona` — write `persona.md` for a workspace.
/// Payload: `{ "workspace_id": string, "text"?: string }`.
pub async fn handle_set_persona(_payload: Value) -> Reply {
    todo!("workspaces redesign: handle_set_persona")
}
