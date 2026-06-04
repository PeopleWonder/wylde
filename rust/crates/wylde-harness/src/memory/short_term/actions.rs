//! `memory.short_term.*` IPC action handlers.
//!
//! The three conversation-scoped working-memory verbs the gpui Chat
//! panel + Gateway consume:
//!
//! * `memory.short_term.get`    → `{ working_memory, conversation_id }`
//! * `memory.short_term.append` → `{ conversation_id, working_memory }`
//! * `memory.short_term.clear`  → `{ cleared, conversation_id }`
//!
//! Reply shapes + error codes match the Python `_memory.py` handlers
//! they replace exactly, so the strangler forward on the Python side
//! (and any direct pipe caller) sees an identical envelope. The actual
//! file IO lives in [`super::store`].

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::store::{self, AppendError};
use crate::api::require_string;

/// `memory.short_term.get` — the rolling working-memory buffer for a
/// conversation. Payload `{ conversation_id }`.
pub async fn handle_get(payload: Value) -> Reply {
    let Some(cid) = require_string(&payload, "conversation_id") else {
        return Reply::err_msg("bad_request", "conversation_id is required");
    };
    match store::get_working_memory(&cid) {
        Ok(working) => Reply::ok(json!({
            "working_memory": working,
            "conversation_id": cid,
        })),
        Err(e) => Reply::err_msg("bad_request", e.0),
    }
}

/// `memory.short_term.append` — append one freeform entry. Payload
/// `{ conversation_id, entry }` where `entry` MUST be a map (matches
/// Python's `entry must be a map` guard). Returns the buffer after the
/// append.
pub async fn handle_append(payload: Value) -> Reply {
    let Some(cid) = require_string(&payload, "conversation_id") else {
        return Reply::err_msg("bad_request", "conversation_id is required");
    };
    let entry = payload.get("entry").cloned().unwrap_or(Value::Null);
    if !entry.is_object() {
        return Reply::err_msg("bad_request", "entry must be a map");
    }
    match store::append_working_memory(&cid, entry) {
        Ok(working) => Reply::ok(json!({
            "conversation_id": cid,
            "working_memory": working,
        })),
        Err(AppendError::InvalidId(e)) => Reply::err_msg("bad_request", e.0),
        Err(AppendError::Io(e)) => Reply::err_msg("io_error", e.to_string()),
    }
}

/// `memory.short_term.clear` — drop the working-memory buffer. Payload
/// `{ conversation_id }`. `cleared` is `false` when there was nothing to
/// clear.
pub async fn handle_clear(payload: Value) -> Reply {
    let Some(cid) = require_string(&payload, "conversation_id") else {
        return Reply::err_msg("bad_request", "conversation_id is required");
    };
    match store::clear_working_memory(&cid) {
        Ok(cleared) => Reply::ok(json!({
            "cleared": cleared,
            "conversation_id": cid,
        })),
        Err(e) => Reply::err_msg("bad_request", e.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::short_term::test_support::TestEnv;

    #[tokio::test]
    async fn get_requires_conversation_id() {
        let _env = TestEnv::new();
        let reply = handle_get(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn append_requires_map_entry() {
        let _env = TestEnv::new();
        let reply = handle_append(json!({"conversation_id": "c1", "entry": "nope"})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn append_get_clear_round_trip_over_actions() {
        let _env = TestEnv::new();
        let cid = "actions_rt";

        let appended = handle_append(json!({
            "conversation_id": cid,
            "entry": {"kind": "tool", "data": {"name": "fs_read"}},
        }))
        .await;
        assert!(appended.ok);
        assert_eq!(appended.data["conversation_id"], cid);
        assert_eq!(appended.data["working_memory"].as_array().unwrap().len(), 1);

        let got = handle_get(json!({"conversation_id": cid})).await;
        assert!(got.ok);
        assert_eq!(got.data["working_memory"].as_array().unwrap().len(), 1);
        assert_eq!(got.data["conversation_id"], cid);

        let cleared = handle_clear(json!({"conversation_id": cid})).await;
        assert!(cleared.ok);
        assert_eq!(cleared.data["cleared"], true);

        let again = handle_clear(json!({"conversation_id": cid})).await;
        assert_eq!(again.data["cleared"], false);
    }

    #[tokio::test]
    async fn get_unknown_conversation_is_empty_not_error() {
        let _env = TestEnv::new();
        let reply = handle_get(json!({"conversation_id": "nope-123"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["working_memory"].as_array().unwrap().len(), 0);
    }
}
