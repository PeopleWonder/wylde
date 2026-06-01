//! `memory.workspaces.*` in-process dispatch.  Mirror of
//! `Core/GUI/src-tauri/src/pipe/memory_workspaces.rs`.

use serde_json::Value;
use wylde_harness::HarnessApi;
use wylde_shared::ipc::Reply;

pub async fn dispatch<A: HarnessApi + ?Sized>(
    api: &A,
    verb: &str,
    payload: Value,
) -> Option<Reply> {
    match verb {
        "memory.workspaces.list" => Some(api.memory_workspaces_list(payload).await),
        "memory.workspaces.recent" => Some(api.memory_workspaces_recent(payload).await),
        "memory.workspaces.get" => Some(api.memory_workspaces_get(payload).await),
        "memory.workspaces.get_mru_limit" => {
            Some(api.memory_workspaces_get_mru_limit(payload).await)
        }
        "memory.workspaces.set_mru_limit" => {
            Some(api.memory_workspaces_set_mru_limit(payload).await)
        }
        "memory.workspaces.get_persona" => Some(api.memory_workspaces_get_persona(payload).await),
        "memory.workspaces.set_persona" => Some(api.memory_workspaces_set_persona(payload).await),
        "memory.workspaces.delete" => Some(api.memory_workspaces_delete(payload).await),
        _ => None,
    }
}
