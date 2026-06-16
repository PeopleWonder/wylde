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

/// `conversations.delete_by_workspace` — sweep every flat-store
/// conversation bound to a workspace (Route 1 deletion complement). Payload
/// `{ workspace_id }`. Returns `{ ok: true, workspace_id, deleted }` where
/// `deleted` is the count of swept docs. A blank `workspace_id` is rejected
/// with `bad_request` so a caller can never aim a mass-delete at the unbound
/// (global) conversations, which carry an empty `workspace_id`.
pub async fn handle_delete_by_workspace(payload: Value) -> Reply {
    let Some(workspace_id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    // Reject a whitespace-only id too — `require_string` only filters the
    // truly-empty string, but a blank id must never reach the sweep (the
    // store no-ops on blank, but failing loudly here keeps a caller from
    // believing a blank-id mass-delete "succeeded with 0").
    if workspace_id.trim().is_empty() {
        return Reply::err_msg("bad_request", "workspace_id must not be blank");
    }
    let deleted = store::delete_by_workspace(&workspace_id);
    Reply::ok(json!({ "ok": true, "workspace_id": workspace_id, "deleted": deleted }))
}

/// `conversations.set_workspace` — re-assign a conversation's workspace
/// (Q4 mutable binding). Payload `{ id, workspace_id? }`; an empty /
/// absent `workspace_id` clears the binding. Upserts the document so the
/// binding sticks even on a freshly minted conversation. Returns the
/// updated document. `bad_request` for a missing / invalid id.
pub async fn handle_set_workspace(payload: Value) -> Reply {
    let Some(cid) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    // An empty workspace_id is meaningful (clears the binding), so read it
    // raw rather than via `require_string`.
    let workspace_id = payload.get("workspace_id").and_then(Value::as_str);
    match store::set_workspace(&cid, workspace_id) {
        Ok(doc) => Reply::ok(doc),
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

/// `conversations.get_active_for_workspace` — the per-workspace last-open
/// pointer (C7). Payload `{ workspace_id }`. Returns `{ workspace_id, id }`
/// with `id == ""` when none recorded. `bad_request` for a missing / blank
/// workspace_id — the global `get_active` owns the unbound surface.
pub async fn handle_get_active_for_workspace(payload: Value) -> Reply {
    let Some(workspace_id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    if workspace_id.trim().is_empty() {
        return Reply::err_msg("bad_request", "workspace_id must not be blank");
    }
    let id = store::get_active_conversation_for_workspace(&workspace_id).unwrap_or_default();
    Reply::ok(json!({ "workspace_id": workspace_id, "id": id }))
}

/// `conversations.set_active_for_workspace` — persist the per-workspace
/// last-open pointer (C7). Payload `{ workspace_id, id? }`; an empty / absent
/// `id` clears that workspace's pointer. Returns `{ workspace_id, id }` (the
/// persisted value, `""` when cleared). `bad_request` for a missing / blank
/// workspace_id.
pub async fn handle_set_active_for_workspace(payload: Value) -> Reply {
    let Some(workspace_id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    if workspace_id.trim().is_empty() {
        return Reply::err_msg("bad_request", "workspace_id must not be blank");
    }
    // An empty id is meaningful (clears the pointer), so read it raw.
    let id = payload.get("id").and_then(Value::as_str);
    let persisted =
        store::set_active_conversation_for_workspace(&workspace_id, id).unwrap_or_default();
    Reply::ok(json!({ "workspace_id": workspace_id, "id": persisted }))
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
    async fn per_workspace_active_round_trips_through_handlers() {
        let _env = TestEnv::new();
        // Unset → "".
        let got = handle_get_active_for_workspace(json!({"workspace_id": "ws-a"})).await;
        assert!(got.ok);
        assert_eq!(got.data["id"], "");
        assert_eq!(got.data["workspace_id"], "ws-a");

        // Set then read back, isolated per workspace.
        let set = handle_set_active_for_workspace(
            json!({"workspace_id": "ws-a", "id": "thread-a"}),
        )
        .await;
        assert!(set.ok);
        assert_eq!(set.data["id"], "thread-a");
        handle_set_active_for_workspace(json!({"workspace_id": "ws-b", "id": "thread-b"})).await;

        let a = handle_get_active_for_workspace(json!({"workspace_id": "ws-a"})).await;
        assert_eq!(a.data["id"], "thread-a", "B must not clobber A");

        // Empty id clears.
        let cleared = handle_set_active_for_workspace(json!({"workspace_id": "ws-a", "id": ""})).await;
        assert_eq!(cleared.data["id"], "");
        let a2 = handle_get_active_for_workspace(json!({"workspace_id": "ws-a"})).await;
        assert_eq!(a2.data["id"], "");
    }

    #[tokio::test]
    async fn per_workspace_active_rejects_blank_workspace() {
        let _env = TestEnv::new();
        let get = handle_get_active_for_workspace(json!({"workspace_id": "  "})).await;
        assert!(!get.ok);
        assert_eq!(get.error.unwrap().code, "bad_request");
        let set = handle_set_active_for_workspace(json!({"id": "x"})).await;
        assert!(!set.ok);
        assert_eq!(set.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn delete_by_workspace_sweeps_and_reports_count() {
        let _env = TestEnv::new();
        seed("w1", json!({"id": "w1", "messages": [], "workspace_id": "ws-z"}));
        seed("w2", json!({"id": "w2", "messages": [], "workspace_id": "ws-z"}));
        seed("keep", json!({"id": "keep", "messages": []}));

        let reply = handle_delete_by_workspace(json!({"workspace_id": "ws-z"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["deleted"], 2);
        assert_eq!(reply.data["workspace_id"], "ws-z");
        // Unbound chat survives; listing now shows only it.
        let listed = handle_list(Value::Null).await;
        assert_eq!(listed.data["count"], 1);
        assert_eq!(listed.data["conversations"][0]["id"], "keep");
    }

    #[tokio::test]
    async fn delete_by_workspace_requires_workspace_id() {
        let _env = TestEnv::new();
        // Blank / absent must be rejected so it can't nuke global chats.
        let missing = handle_delete_by_workspace(json!({})).await;
        assert!(!missing.ok);
        assert_eq!(missing.error.unwrap().code, "bad_request");
        let blank = handle_delete_by_workspace(json!({"workspace_id": "   "})).await;
        assert!(!blank.ok);
        assert_eq!(blank.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn set_workspace_upserts_and_returns_doc() {
        let _env = TestEnv::new();
        let missing_id = handle_set_workspace(json!({})).await;
        assert!(!missing_id.ok);
        assert_eq!(missing_id.error.unwrap().code, "bad_request");

        let set = handle_set_workspace(json!({"id": "conv-w", "workspace_id": "ws-99"})).await;
        assert!(set.ok);
        assert_eq!(set.data["workspace_id"], "ws-99");
        // Mutable re-assign.
        let reassigned =
            handle_set_workspace(json!({"id": "conv-w", "workspace_id": "ws-77"})).await;
        assert_eq!(reassigned.data["workspace_id"], "ws-77");
        // Visible to get.
        let got = handle_get(json!({"id": "conv-w"})).await;
        assert_eq!(got.data["workspace_id"], "ws-77");
    }

    #[tokio::test]
    async fn new_then_set_workspace_binds_and_lists() {
        // C5 real-path: the exact verb sequence the Docked dock's "+ New" runs
        // over IPC — `conversations.new` mints an id, then
        // `conversations.set_workspace` binds it — must land a document on disk
        // that (a) carries the `workspace_id` and (b) is visible to
        // `conversations.list` scoped to that workspace. An *unbound* mint has no
        // file until its first turn; the bind's upsert is what makes a brand-new
        // bound thread appear in the workspace's scoped rail immediately.
        let _env = TestEnv::new();

        // 1. Mint — no file yet, so the list is still empty.
        let minted = handle_new(Value::Null).await;
        assert!(minted.ok);
        let id = minted.data["id"].as_str().unwrap().to_owned();
        assert!(!id.is_empty());
        let before = handle_list(Value::Null).await;
        assert_eq!(before.data["count"], 0, "an unbound mint writes no file yet");

        // 2. Bind — upserts the doc with the workspace_id.
        let bound = handle_set_workspace(json!({"id": id, "workspace_id": "ws-c5"})).await;
        assert!(bound.ok);
        assert_eq!(bound.data["workspace_id"], "ws-c5");

        // 3. Read-back: the persisted doc carries the binding...
        let got = handle_get(json!({"id": id})).await;
        assert!(got.ok);
        assert_eq!(got.data["workspace_id"], "ws-c5");

        // ...and it now appears in the list with its workspace_id projected, so
        // the GUI's per-workspace filter (C4) will pick it up immediately.
        let listed = handle_list(Value::Null).await;
        assert_eq!(listed.data["count"], 1, "the bound thread is now on disk");
        let row = &listed.data["conversations"][0];
        assert_eq!(row["id"], id);
        assert_eq!(
            row["workspace_id"], "ws-c5",
            "the scoped rail can filter this thread into workspace ws-c5",
        );
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
