//! `notes_api.rs` — the harness compat-shim proxy for the
//! `workspaces.notes.*` verbs (Thought Bubble System, Slice 0c).
//!
//! **Conceptual path:** `Core/Harness/Workspaces/notes_api`.
//!
//! The workspace-notes tier moved to the `wylde-workspaces` service. The
//! harness pipe still answers `workspaces.notes.*` — consumers are pinned to
//! it until Slice 0d repoints them — but each handler forwards to the running
//! service and falls back to in-process execution (through the
//! `wylde_workspaces` lib, the *same* code the service runs) when the service
//! isn't reachable. Same shape as [`super::api`]; the shared forwarding
//! helpers live there.

use serde_json::Value;
use wylde_shared::ipc::Reply;
use wylde_workspaces::notes::api as local;

use super::api::forward;

/// `workspaces.notes.list`.
pub async fn handle_list(payload: Value) -> Reply {
    match forward("workspaces.notes.list", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_list(payload).await,
    }
}

/// `workspaces.notes.add`.
pub async fn handle_add(payload: Value) -> Reply {
    match forward("workspaces.notes.add", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_add(payload).await,
    }
}

/// `workspaces.notes.update`.
pub async fn handle_update(payload: Value) -> Reply {
    match forward("workspaces.notes.update", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_update(payload).await,
    }
}

/// `workspaces.notes.delete`.
pub async fn handle_delete(payload: Value) -> Reply {
    match forward("workspaces.notes.delete", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_delete(payload).await,
    }
}

/// `workspaces.notes.search`.
pub async fn handle_search(payload: Value) -> Reply {
    match forward("workspaces.notes.search", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_search(payload).await,
    }
}

/// `workspaces.notes.propose`.
pub async fn handle_propose(payload: Value) -> Reply {
    match forward("workspaces.notes.propose", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_propose(payload).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;
    use serde_json::json;

    // `TestEnv` points the shim at a dead pipe, so every call falls back
    // in-process (through the `wylde_workspaces` lib) against the test's
    // isolated `WYLDE_DATA_DIR`.

    #[tokio::test]
    async fn add_then_list_falls_back_in_process() {
        let _env = TestEnv::new();
        let ws = "ws-notes-shim-000000";
        let added = handle_add(json!({ "workspace_id": ws, "text": "uses tokio" })).await;
        assert!(added.ok, "add (fallback) failed: {:?}", added.error);
        let id = added.data["id"].as_str().unwrap().to_owned();

        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert!(listed.ok);
        assert_eq!(listed.data["count"], 1);
        assert_eq!(listed.data["notes"][0]["id"], id);
    }

    #[tokio::test]
    async fn update_unknown_falls_back_and_surfaces_not_found() {
        let _env = TestEnv::new();
        let reply =
            handle_update(json!({ "workspace_id": "ws", "id": "ghost", "text": "x" })).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn add_bad_request_falls_back_and_surfaces_error() {
        let _env = TestEnv::new();
        let reply = handle_add(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }
}
