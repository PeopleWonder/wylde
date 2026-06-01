//! `memory.workspaces.*` IPC action handlers.
//!
//! Wire surface introduced by slice 7.A. Distinct from the Python
//! `rag.workspaces.*` actions: those stay canonical and continue to
//! handle indexing-side traffic. These new actions cover the
//! registry-only half so callers (parity tests, the future strangler
//! forwarder in `_rag_workspaces.py`) can exercise the Rust store
//! without depending on LanceDB.
//!
//! Each handler returns a `Reply`. `Reply::ok(data)` for success,
//! `Reply::err_msg(code, msg)` for errors. Codes match Python's:
//! `bad_request` for missing/invalid payload fields, `not_found` for
//! unknown workspace ids, otherwise nothing.

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::mru;
use super::store::{
    self, delete_workspace, get_persona, get_workspace, list_workspaces, recent_workspaces,
    set_persona, touch_activated, ActivationError,
};

/// `memory.workspaces.list` — every workspace in MRU order.
pub async fn handle_list(_payload: Value) -> Reply {
    let workspaces: Vec<Value> = list_workspaces().iter().map(|w| w.to_value()).collect();
    Reply::ok(json!({ "workspaces": workspaces }))
}

/// `memory.workspaces.recent` — first N in MRU order.
/// Payload: `{ "limit"?: number }`. Default = current MRU cap.
pub async fn handle_recent(payload: Value) -> Reply {
    let cap = mru::get_mru_limit();
    let n = payload
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(cap)
        .min(cap);
    let workspaces: Vec<Value> = recent_workspaces(Some(n))
        .iter()
        .map(|w| w.to_value())
        .collect();
    Reply::ok(json!({ "workspaces": workspaces }))
}

/// `memory.workspaces.get` — one workspace by id.
/// Payload: `{ "workspace_id": string }`.
pub async fn handle_get(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    match get_workspace(&wsid) {
        Some(w) => Reply::ok(w.to_value()),
        None => Reply::err_msg("not_found", format!("workspace {wsid:?} not found")),
    }
}

/// `memory.workspaces.get_mru_limit` — current cap + bounds.
pub async fn handle_get_mru_limit(_payload: Value) -> Reply {
    Reply::ok(json!({
        "limit": mru::get_mru_limit(),
        "min": mru::MRU_LIMIT_MIN,
        "max": mru::MRU_LIMIT_MAX,
        "default": mru::MRU_LIMIT_DEFAULT,
    }))
}

/// `memory.workspaces.set_mru_limit` — persist a new cap; immediately
/// evicts overflow.
/// Payload: `{ "limit": number }`.
pub async fn handle_set_mru_limit(payload: Value) -> Reply {
    let Some(raw) = payload.get("limit") else {
        return Reply::err_msg("bad_request", "limit is required");
    };
    match mru::set_mru_limit(raw) {
        Ok(n) => {
            let workspaces: Vec<Value> = list_workspaces().iter().map(|w| w.to_value()).collect();
            Reply::ok(json!({ "limit": n, "workspaces": workspaces }))
        }
        Err(e) => Reply::err_msg("bad_request", e.to_string()),
    }
}

/// `memory.workspaces.get_persona` — persona text for a workspace.
/// Payload: `{ "workspace_id": string }`.
pub async fn handle_get_persona(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    Reply::ok(json!({
        "workspace_id": wsid,
        "persona": get_persona(&wsid),
    }))
}

/// `memory.workspaces.set_persona` — persist persona text.
/// Payload: `{ "workspace_id": string, "text"?: string }`.
pub async fn handle_set_persona(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    let ok = set_persona(&wsid, text);
    Reply::ok(json!({ "ok": ok, "workspace_id": wsid }))
}

/// `memory.workspaces.delete` — explicit delete (registry + index +
/// workspace memory).
/// Payload: `{ "workspace_id": string }`.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let ok = delete_workspace(&wsid);
    Reply::ok(json!({ "ok": ok, "workspace_id": wsid }))
}

/// `memory.workspaces.touch_activated` — registry-side activation
/// (moves to head, evicts tail). NOT a full activation: indexing is
/// 7.B's job. Exposed so parity tests can exercise the MRU bookkeeping
/// without LanceDB.
/// Payload: `{ "path": string }`.
pub async fn handle_touch_activated(payload: Value) -> Reply {
    let Some(path) = require_string(&payload, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    match touch_activated(&path) {
        Ok(w) => Reply::ok(w.to_value()),
        Err(ActivationError::NotFound { .. } | ActivationError::NotADirectory { .. }) => {
            // Python's `rag.workspaces.activate` returns `bad_request`
            // for both — match.
            Reply::err_msg("bad_request", format!("invalid workspace path: {path:?}"))
        }
    }
}

fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

// `store` re-export kept so `super::actions::store::Workspace` is
// reachable from test modules without an extra `use`.
#[allow(unused_imports)]
use store as _store_namespace_marker;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::workspaces::test_support::TestEnv;
    use tempfile::tempdir;

    #[tokio::test]
    async fn list_returns_workspaces_in_mru_order() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        for n in ["a", "b"] {
            let p = td.path().join(n);
            std::fs::create_dir(&p).unwrap();
            touch_activated(p.to_str().unwrap()).unwrap();
        }
        let reply = handle_list(Value::Null).await;
        assert!(reply.ok);
        let arr = reply.data["workspaces"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr[0]["path"].as_str().unwrap().ends_with("b"));
    }

    #[tokio::test]
    async fn recent_respects_explicit_limit_payload() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        for i in 0..3 {
            let p = td.path().join(format!("r{i}"));
            std::fs::create_dir(&p).unwrap();
            touch_activated(p.to_str().unwrap()).unwrap();
        }
        let reply = handle_recent(json!({"limit": 2})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["workspaces"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_returns_not_found_for_unknown_id() {
        let _env = TestEnv::new();
        let reply = handle_get(json!({"workspace_id": "nope-000000"})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn get_returns_bad_request_when_workspace_id_missing() {
        let reply = handle_get(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn get_mru_limit_returns_bounds_and_default() {
        let _env = TestEnv::new();
        let reply = handle_get_mru_limit(Value::Null).await;
        assert!(reply.ok);
        assert_eq!(reply.data["min"], mru::MRU_LIMIT_MIN);
        assert_eq!(reply.data["max"], mru::MRU_LIMIT_MAX);
        assert_eq!(reply.data["default"], mru::MRU_LIMIT_DEFAULT);
    }

    #[tokio::test]
    async fn set_mru_limit_validates_and_returns_workspaces() {
        let _env = TestEnv::new();
        let reply = handle_set_mru_limit(json!({"limit": 9})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["limit"], 9);
        assert!(reply.data["workspaces"].is_array());
    }

    #[tokio::test]
    async fn set_mru_limit_rejects_out_of_range() {
        let _env = TestEnv::new();
        let reply = handle_set_mru_limit(json!({"limit": 999})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn set_mru_limit_rejects_missing_limit() {
        let reply = handle_set_mru_limit(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn persona_round_trip_through_actions() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("persona");
        std::fs::create_dir(&p).unwrap();
        let w = touch_activated(p.to_str().unwrap()).unwrap();

        let set_reply = handle_set_persona(json!({
            "workspace_id": w.id,
            "text": "speak only in haiku",
        }))
        .await;
        assert!(set_reply.ok);

        let get_reply = handle_get_persona(json!({"workspace_id": w.id})).await;
        assert!(get_reply.ok);
        assert_eq!(get_reply.data["persona"], "speak only in haiku");
    }

    #[tokio::test]
    async fn delete_removes_workspace() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("del");
        std::fs::create_dir(&p).unwrap();
        let w = touch_activated(p.to_str().unwrap()).unwrap();
        let reply = handle_delete(json!({"workspace_id": w.id})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["ok"], true);
        assert!(get_workspace(&w.id).is_none());
    }

    #[tokio::test]
    async fn touch_activated_rejects_missing_path() {
        let _env = TestEnv::new();
        let reply = handle_touch_activated(json!({
            "path": std::env::temp_dir()
                .join("no-such-wylde-test-dir-honest-67890")
                .to_string_lossy(),
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }
}
