//! `api.rs` — the `workspaces.conversations.*` IPC verb surface.
//!
//! **Conceptual path:** `Core/Workspaces/Conversations/api`.
//!
//! Slice 0c moves workspace-scoped conversation *storage* into this service
//! (per-workspace bundle dirs) and exposes the lifecycle read verbs over the
//! new pipe. The verb set mirrors the lifecycle subset the harness flat store
//! already surfaces (`list` / `get` / `delete`) — every workspace verb takes
//! an explicit `workspace_id` to keep the scope boundary unambiguous (plan §3
//! — workspace conversations cannot leak across workspaces).
//!
//! Verb set (Slice 0c):
//!
//! * `workspaces.conversations.list`   — metadata for one workspace.
//! * `workspaces.conversations.get`    — the full document.
//! * `workspaces.conversations.delete` — remove one conversation.
//!
//! The richer surface from the Build Order (`search` / `summary` / `tags` /
//! `export` / `import`) is deferred to its owning slices (E / J): those verbs
//! don't exist in the harness API today, and the foundation-first pyramid
//! introduces them where they're consumed. Registration on the pipe lands in
//! [`crate::action_dispatch`].

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::store::{self, ReadError};

fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// `workspaces.conversations.list` — lightweight metadata for one
/// workspace's conversations, newest-first. Payload `{ workspace_id }`.
/// Returns `{ workspace_id, conversations, count }`.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let metas = store::list_conversations(&ws);
    let count = metas.len();
    Reply::ok(json!({ "workspace_id": ws, "conversations": metas, "count": count }))
}

/// `workspaces.conversations.get` — the full conversation document. Payload
/// `{ workspace_id, id }`. `bad_request` for a missing/invalid id,
/// `not_found` when absent in that workspace.
pub async fn handle_get(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::read_conversation(&ws, &id) {
        Ok(doc) => Reply::ok(doc),
        Err(ReadError::InvalidId(e)) => Reply::err_msg("bad_request", e.0),
        Err(ReadError::NotFound(e)) => Reply::err_msg("not_found", e.0),
    }
}

/// `workspaces.conversations.delete` — remove one conversation. Payload
/// `{ workspace_id, id }`. Returns `{ ok, id }` (`ok` false when absent).
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::delete_conversation(&ws, &id) {
        Ok(deleted) => Reply::ok(json!({ "ok": deleted, "id": id })),
        Err(e) => Reply::err_msg("bad_request", e.0),
    }
}

/// `workspaces.conversations.refresh_summary` — persist an LLM summary +
/// embedding the harness computed for a workspace conversation (Slice E
/// parity). Payload `{ workspace_id, conversation_id, summary, embedding,
/// topic_tags?, summary_msg_count? }`. The service validates and folds the
/// derived fields into the stored doc (without bumping `updated_at`), so the
/// scoped semantic search ranks workspace conversations by the same cosine
/// path standalone ones already use.
///
/// Generation (the Ollama summary + embed) is deliberately the harness's job:
/// the service has no Ollama client, and this keeps the embed/LLM pipeline in
/// one place. The verb is the validate + persist landing point.
pub async fn handle_refresh_summary(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    // Accept `conversation_id` (the verb's documented name) or `id` (the shape
    // the sibling get/delete verbs use) so either caller resolves.
    let Some(id) = require_string(&payload, "conversation_id")
        .or_else(|| require_string(&payload, "id"))
    else {
        return Reply::err_msg("bad_request", "conversation_id is required");
    };
    let Some(summary) = payload.get("summary").and_then(Value::as_str) else {
        return Reply::err_msg("bad_request", "summary is required");
    };
    let summary = summary.trim();
    if summary.is_empty() {
        return Reply::err_msg("bad_request", "summary must be non-empty");
    }
    // Embedding: a non-empty array of finite numbers.
    let Some(embedding) = payload.get("embedding").and_then(Value::as_array) else {
        return Reply::err_msg("bad_request", "embedding must be an array");
    };
    if embedding.is_empty() {
        return Reply::err_msg("bad_request", "embedding must be non-empty");
    }
    if !embedding.iter().all(|v| v.as_f64().is_some_and(f64::is_finite)) {
        return Reply::err_msg("bad_request", "embedding must contain only finite numbers");
    }

    let tags: Vec<Value> = payload
        .get("topic_tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| json!(s))
                .collect()
        })
        .unwrap_or_default();
    let msg_count = payload
        .get("summary_msg_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut fields = serde_json::Map::new();
    fields.insert("auto_summary".to_owned(), json!(summary));
    fields.insert("topic_tags".to_owned(), Value::Array(tags));
    fields.insert("embedding".to_owned(), Value::Array(embedding.clone()));
    fields.insert("summary_msg_count".to_owned(), json!(msg_count));

    match store::merge_fields(&ws, &id, fields) {
        Ok(_doc) => Reply::ok(json!({ "ok": true, "id": id })),
        Err(ReadError::InvalidId(e)) => Reply::err_msg("bad_request", e.0),
        Err(ReadError::NotFound(e)) => Reply::err_msg("not_found", e.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    fn seed(ws: &str, doc: Value) {
        let map = doc.as_object().unwrap().clone();
        store::save_conversation(ws, &map).unwrap();
    }

    #[tokio::test]
    async fn list_get_delete_round_trip() {
        let _env = TestEnv::new();
        let ws = "ws-conv-api-000000";
        seed(ws, json!({"id": "c1", "title": "Hi", "updated_at": 5, "messages": [], "workspace_id": ws}));

        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert!(listed.ok);
        assert_eq!(listed.data["count"], 1);
        assert_eq!(listed.data["conversations"][0]["id"], "c1");

        let got = handle_get(json!({ "workspace_id": ws, "id": "c1" })).await;
        assert!(got.ok);
        assert_eq!(got.data["title"], "Hi");

        let del = handle_delete(json!({ "workspace_id": ws, "id": "c1" })).await;
        assert_eq!(del.data["ok"], true);
        let empty = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(empty.data["count"], 0);
    }

    #[tokio::test]
    async fn get_requires_ids_then_404s() {
        let _env = TestEnv::new();
        let no_ws = handle_get(json!({ "id": "c1" })).await;
        assert_eq!(no_ws.error.unwrap().code, "bad_request");
        let no_id = handle_get(json!({ "workspace_id": "ws" })).await;
        assert_eq!(no_id.error.unwrap().code, "bad_request");
        let missing = handle_get(json!({ "workspace_id": "ws", "id": "ghost" })).await;
        assert_eq!(missing.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn get_invalid_id_is_bad_request() {
        let _env = TestEnv::new();
        let r = handle_get(json!({ "workspace_id": "ws", "id": "bad/slash" })).await;
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn refresh_summary_persists_and_is_searchable() {
        let _env = TestEnv::new();
        let ws = "ws-refresh-000000";
        seed(ws, json!({"id": "c1", "title": "T", "updated_at": 9, "workspace_id": ws,
                        "messages": [{"role": "user", "content": "how do anchors work"}]}));

        let r = handle_refresh_summary(json!({
            "workspace_id": ws,
            "conversation_id": "c1",
            "summary": "Explained how anchors work.",
            "topic_tags": ["anchors", "vocab"],
            "embedding": [0.1, 0.2, 0.3],
            "summary_msg_count": 1,
        }))
        .await;
        assert!(r.ok, "refresh ok: {:?}", r.error);

        // The derived fields are now on the stored doc — what the cosine
        // search ranks on — and updated_at is untouched.
        let got = handle_get(json!({ "workspace_id": ws, "id": "c1" })).await;
        assert_eq!(got.data["auto_summary"], "Explained how anchors work.");
        assert_eq!(got.data["topic_tags"], json!(["anchors", "vocab"]));
        assert_eq!(got.data["embedding"].as_array().unwrap().len(), 3);
        assert_eq!(got.data["updated_at"], 9);
    }

    #[tokio::test]
    async fn refresh_summary_validates_inputs() {
        let _env = TestEnv::new();
        let ws = "ws-refresh-val-000000";
        seed(ws, json!({"id": "c1", "messages": [], "workspace_id": ws}));

        // Missing embedding.
        let no_embed = handle_refresh_summary(json!({
            "workspace_id": ws, "conversation_id": "c1", "summary": "s"
        }))
        .await;
        assert_eq!(no_embed.error.unwrap().code, "bad_request");

        // Empty summary.
        let empty_summary = handle_refresh_summary(json!({
            "workspace_id": ws, "conversation_id": "c1", "summary": "  ", "embedding": [0.1]
        }))
        .await;
        assert_eq!(empty_summary.error.unwrap().code, "bad_request");

        // Non-finite embedding value (NaN serialises as null → rejected).
        let bad_embed = handle_refresh_summary(json!({
            "workspace_id": ws, "conversation_id": "c1", "summary": "s", "embedding": ["x"]
        }))
        .await;
        assert_eq!(bad_embed.error.unwrap().code, "bad_request");

        // Unknown conversation.
        let missing = handle_refresh_summary(json!({
            "workspace_id": ws, "conversation_id": "ghost", "summary": "s", "embedding": [0.1]
        }))
        .await;
        assert_eq!(missing.error.unwrap().code, "not_found");
    }
}
