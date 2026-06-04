//! `conversations.*` IPC action handlers.
//!
//! The conversation-lifecycle verbs the gpui Chat panel's switcher
//! consumes:
//!
//! * `conversations.new`        → `{ id }`
//! * `conversations.list`       → `{ conversations, count }`
//! * `conversations.get`        → the full conversation document
//! * `conversations.delete`     → `{ ok, id }`
//! * `conversations.get_active` → `{ id }` (`""`/absent → no selection)
//! * `conversations.set_active` → `{ id }`
//!
//! Reply shapes + error codes match the Python `_conversations.py`
//! handlers they replace (`new`/`list`/`get`/`delete`), so the strangler
//! forward on the Python side sees an identical envelope. The
//! `*_active` pair is net-new wire surface for Slice B's persistence; it
//! mirrors `models.set_active`'s `{ model }` ↔ `{ id }` shape. File IO
//! lives in [`super::store`].

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::store::{self, ReadError};
use crate::api::require_string;

/// `conversations.new` — mint a fresh, sortable, filename-safe id. No
/// payload. Returns `{ id }`.
pub async fn handle_new(_payload: Value) -> Reply {
    Reply::ok(json!({ "id": store::new_conversation_id() }))
}

/// `conversations.list` — lightweight metadata for every saved chat,
/// newest-first. No payload. Returns `{ conversations, count }`.
pub async fn handle_list(_payload: Value) -> Reply {
    let metas = store::list_conversations();
    let count = metas.len();
    Reply::ok(json!({ "conversations": metas, "count": count }))
}

/// `conversations.get` — the full conversation document. Payload `{ id }`.
/// `bad_request` for a missing / invalid id, `not_found` when absent.
pub async fn handle_get(payload: Value) -> Reply {
    let Some(cid) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::read_conversation(&cid) {
        Ok(doc) => Reply::ok(doc),
        Err(ReadError::InvalidId(e)) => Reply::err_msg("bad_request", e.0),
        Err(ReadError::NotFound(e)) => Reply::err_msg("not_found", e.0),
    }
}

/// `conversations.delete` — remove a conversation file. Payload `{ id }`.
/// Returns `{ ok, id }` where `ok` is `false` when the file was already
/// absent. `bad_request` for an invalid id.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(cid) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::delete_conversation(&cid) {
        Ok(deleted) => Reply::ok(json!({ "ok": deleted, "id": cid })),
        Err(e) => Reply::err_msg("bad_request", e.0),
    }
}

/// `conversations.get_active` — the persisted active-conversation
/// selection, or `""` when none. No payload. Returns `{ id }`.
pub async fn handle_get_active(_payload: Value) -> Reply {
    let id = store::get_active_conversation().unwrap_or_default();
    Reply::ok(json!({ "id": id }))
}

/// `conversations.set_active` — persist the user's active-conversation
/// selection. Payload `{ id }`; an empty / absent / non-string `id`
/// clears the selection. Returns `{ id }` (the persisted value, `""` when
/// cleared) — mirroring `models.set_active`'s tolerant shape.
pub async fn handle_set_active(payload: Value) -> Reply {
    // Unlike the other verbs, an empty id is meaningful here (it clears
    // the selection), so we read the raw string rather than `require_string`
    // (which treats empty as absent). A non-string is treated as "clear".
    let id = payload.get("id").and_then(Value::as_str);
    let persisted = store::set_active_conversation(id).unwrap_or_default();
    Reply::ok(json!({ "id": persisted }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::conversations::test_support::TestEnv;

    fn seed(cid: &str, doc: Value) {
        // Route through the public list/get path by writing the file the
        // same way the store reads it.
        let path = crate::memory::common::conversations_dir().join(format!("{cid}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn new_returns_unique_ids() {
        let _env = TestEnv::new();
        let a = handle_new(Value::Null).await;
        let b = handle_new(Value::Null).await;
        assert!(a.ok && b.ok);
        let ida = a.data["id"].as_str().unwrap();
        let idb = b.data["id"].as_str().unwrap();
        assert!(!ida.is_empty());
        assert_ne!(ida, idb);
    }

    #[tokio::test]
    async fn list_counts_and_orders() {
        let _env = TestEnv::new();
        let empty = handle_list(Value::Null).await;
        assert!(empty.ok);
        assert_eq!(empty.data["count"], 0);

        seed("a", json!({"id": "a", "updated_at": 10, "messages": []}));
        seed("b", json!({"id": "b", "updated_at": 20, "messages": []}));
        let listed = handle_list(Value::Null).await;
        assert_eq!(listed.data["count"], 2);
        let arr = listed.data["conversations"].as_array().unwrap();
        assert_eq!(arr[0]["id"], "b", "newest first");
    }

    #[tokio::test]
    async fn get_requires_id_then_finds_or_404s() {
        let _env = TestEnv::new();
        let missing_id = handle_get(json!({})).await;
        assert!(!missing_id.ok);
        assert_eq!(missing_id.error.unwrap().code, "bad_request");

        let not_found = handle_get(json!({"id": "ghost"})).await;
        assert!(!not_found.ok);
        assert_eq!(not_found.error.unwrap().code, "not_found");

        seed("c", json!({"id": "c", "title": "Hi", "messages": []}));
        let found = handle_get(json!({"id": "c"})).await;
        assert!(found.ok);
        assert_eq!(found.data["title"], "Hi");
    }

    #[tokio::test]
    async fn get_invalid_id_is_bad_request_not_404() {
        let _env = TestEnv::new();
        let reply = handle_get(json!({"id": "bad/slash"})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn delete_reports_ok_flag() {
        let _env = TestEnv::new();
        seed("d", json!({"id": "d", "messages": []}));
        let first = handle_delete(json!({"id": "d"})).await;
        assert!(first.ok);
        assert_eq!(first.data["ok"], true);
        assert_eq!(first.data["id"], "d");

        let again = handle_delete(json!({"id": "d"})).await;
        assert_eq!(again.data["ok"], false);
    }

    #[tokio::test]
    async fn delete_requires_id() {
        let _env = TestEnv::new();
        let reply = handle_delete(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn active_get_set_round_trips() {
        let _env = TestEnv::new();
        let unset = handle_get_active(Value::Null).await;
        assert!(unset.ok);
        assert_eq!(unset.data["id"], "");

        let set = handle_set_active(json!({"id": "conv-1"})).await;
        assert!(set.ok);
        assert_eq!(set.data["id"], "conv-1");

        let got = handle_get_active(Value::Null).await;
        assert_eq!(got.data["id"], "conv-1");

        // Empty id clears.
        let cleared = handle_set_active(json!({"id": ""})).await;
        assert_eq!(cleared.data["id"], "");
        let got2 = handle_get_active(Value::Null).await;
        assert_eq!(got2.data["id"], "");
    }
}
