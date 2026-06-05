//! `workspaces.*` in-process dispatch (config-file-backed redesign).
//!
//! Replaces the retired `memory.workspaces.*` in-process dispatch. The
//! one read verb (`list_mru`) and the write verbs short-circuit straight
//! to [`wylde_harness::HarnessApi`] when the GUI hosts the harness
//! in-process; unknown verbs return `None` so the caller falls through to
//! the over-the-wire path.

use serde_json::Value;
use wylde_harness::HarnessApi;
use wylde_shared::ipc::Reply;

pub async fn dispatch<A: HarnessApi + ?Sized>(
    api: &A,
    verb: &str,
    payload: Value,
) -> Option<Reply> {
    match verb {
        "workspaces.set_active" => Some(api.workspaces_set_active(payload).await),
        "workspaces.create" => Some(api.workspaces_create(payload).await),
        "workspaces.update" => Some(api.workspaces_update(payload).await),
        "workspaces.delete" => Some(api.workspaces_delete(payload).await),
        "workspaces.set_persona" => Some(api.workspaces_set_persona(payload).await),
        "workspaces.list_mru" => Some(api.workspaces_list_mru(payload).await),
        _ => None,
    }
}
