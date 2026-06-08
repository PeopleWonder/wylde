//! `conversations_api.rs` — the harness compat-shim proxy for the
//! `workspaces.conversations.*` verbs (Thought Bubble System, Slice 0c).
//!
//! **Conceptual path:** `Core/Harness/Workspaces/conversations_api`.
//!
//! Workspace-scoped conversation storage moved to the `wylde-workspaces`
//! service (per-workspace bundle dirs). The harness pipe answers the
//! `workspaces.conversations.*` lifecycle verbs by forwarding to the service,
//! falling back to in-process execution via the `wylde_workspaces` lib when
//! the service is unreachable. Same forward-then-fall-back shape as
//! [`super::api`].
//!
//! **Not to be confused with** the harness's own `conversations.*` verbs
//! ([`crate::memory::conversations`]): those operate on the *flat* store and
//! own **standalone** conversations (`workspace_id == None`), which stay
//! harness-owned and are untouched by this slice.

use serde_json::Value;
use wylde_shared::ipc::Reply;
use wylde_workspaces::conversations::api as local;

use super::api::forward;

/// `workspaces.conversations.list`.
pub async fn handle_list(payload: Value) -> Reply {
    match forward("workspaces.conversations.list", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_list(payload).await,
    }
}

/// `workspaces.conversations.get`.
pub async fn handle_get(payload: Value) -> Reply {
    match forward("workspaces.conversations.get", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_get(payload).await,
    }
}

/// `workspaces.conversations.delete`.
pub async fn handle_delete(payload: Value) -> Reply {
    match forward("workspaces.conversations.delete", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_delete(payload).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;
    use serde_json::json;

    // `TestEnv` points the shim at a dead pipe → every call falls back
    // in-process against the test's isolated `WYLDE_DATA_DIR`.

    #[tokio::test]
    async fn list_get_delete_fall_back_in_process() {
        let _env = TestEnv::new();
        let ws = "ws-conv-shim-000000";
        // Seed a workspace conversation directly in the service layout (the
        // in-process fallback reads the same per-workspace dir).
        let map = json!({"id": "c1", "title": "Shim", "messages": [], "workspace_id": ws})
            .as_object()
            .unwrap()
            .clone();
        wylde_workspaces::conversations::store::save_conversation(ws, &map).unwrap();

        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert!(listed.ok);
        assert_eq!(listed.data["count"], 1);

        let got = handle_get(json!({ "workspace_id": ws, "id": "c1" })).await;
        assert!(got.ok);
        assert_eq!(got.data["title"], "Shim");

        let del = handle_delete(json!({ "workspace_id": ws, "id": "c1" })).await;
        assert_eq!(del.data["ok"], true);
    }

    #[tokio::test]
    async fn get_missing_falls_back_and_surfaces_not_found() {
        let _env = TestEnv::new();
        let reply = handle_get(json!({ "workspace_id": "ws", "id": "ghost" })).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_found");
    }

    /// Standalone conversations (`workspace_id == None`) live in the harness
    /// flat store and must NEVER surface through the workspace-conversation
    /// service path — the scope boundary the slice promises. A standalone
    /// chat written via the harness flat verbs is invisible to
    /// `workspaces.conversations.list` for any workspace.
    #[tokio::test]
    async fn standalone_conversations_never_reach_the_workspace_store() {
        let _env = TestEnv::new();
        // Write a standalone conversation through the harness flat store
        // (the untouched `conversations.*` surface — no workspace_id).
        let path = crate::memory::common::conversations_dir().join("solo.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "id": "solo", "title": "Standalone", "updated_at": 1, "messages": []
            }))
            .unwrap(),
        )
        .unwrap();

        // The harness flat store still lists it (standalone untouched).
        let flat = crate::memory::conversations::actions::handle_list(Value::Null).await;
        assert!(flat.ok);
        assert_eq!(flat.data["count"], 1);
        assert_eq!(flat.data["conversations"][0]["id"], "solo");

        // The workspace-conversation service path (via the in-process
        // fallback) never sees it — for ANY workspace id.
        for ws in ["", "any-ws", "solo"] {
            let listed = handle_list(json!({ "workspace_id": ws })).await;
            if ws.is_empty() {
                // empty workspace_id is a bad_request, not a leak.
                assert!(!listed.ok);
            } else {
                assert!(listed.ok);
                assert_eq!(listed.data["count"], 0, "standalone leaked into ws {ws:?}");
            }
        }
    }
}
