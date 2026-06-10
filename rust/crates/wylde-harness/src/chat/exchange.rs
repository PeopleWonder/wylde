//! Conversation export / import dispatch (TBS Slice J) — the harness face
//! of the escape hatch.
//!
//! One verb pair for the GUI regardless of where a conversation lives,
//! following the Slice E dispatch pattern:
//!
//!   * payload carries a `workspace_id` → forward over the pipe to the
//!     wylde-workspaces `chat.export` / `chat.import` verbs (the Appendix A
//!     owners);
//!   * no `workspace_id` → the standalone flat store, served in-process
//!     with the SAME portable envelope (`wylde_shared::conversation_export`)
//!     so a standalone export imports into a workspace and vice versa.
//!
//! Import collisions are `already_exists` unless `overwrite: true` — both
//! stores, same rule, nothing silently replaced.

use serde_json::{json, Value};
use wylde_shared::conversation_export as envelope;
use wylde_shared::ipc::Reply;
use wylde_workspaces_client::{ClientError, WorkspacesClient};

use crate::memory::conversations::store as conv_store;

/// The service name override hook the search module also honours.
fn workspaces_service() -> String {
    std::env::var("WYLDE_WORKSPACES_SERVICE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wylde-workspaces".to_owned())
}

fn opt_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// `chat.export` — `{conversation_id, workspace_id?}`. Reply `{export, id}`.
pub async fn handle_export(payload: Value) -> Reply {
    let Some(id) = opt_string(&payload, "conversation_id").or_else(|| opt_string(&payload, "id"))
    else {
        return Reply::err_msg("bad_request", "conversation_id is required");
    };
    match opt_string(&payload, "workspace_id") {
        // Workspace conversation — the service owns it.
        Some(ws) => {
            let client = WorkspacesClient::for_service(workspaces_service());
            match client.chat_export(&ws, &id).await {
                Ok(data) => Reply::ok(data),
                Err(e) => Reply::err_msg(client_code(&e), e.message.clone()),
            }
        }
        // Standalone — the flat store, in-process.
        None => match conv_store::read_conversation(&id) {
            Ok(doc) => Reply::ok(json!({
                "export": envelope::wrap("standalone", doc),
                "id": id,
            })),
            Err(conv_store::ReadError::InvalidId(e)) => Reply::err_msg("bad_request", e.0),
            Err(conv_store::ReadError::NotFound(e)) => Reply::err_msg("not_found", e.0),
        },
    }
}

/// `chat.import` — `{export, workspace_id?, overwrite?}`. Reply
/// `{imported, workspace_id?}`.
pub async fn handle_import(payload: Value) -> Reply {
    let Some(envelope_value) = payload.get("export") else {
        return Reply::err_msg("bad_request", "export (the envelope object) is required");
    };
    let overwrite = payload
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match opt_string(&payload, "workspace_id") {
        Some(ws) => {
            let client = WorkspacesClient::for_service(workspaces_service());
            match client
                .chat_import(&ws, envelope_value.clone(), overwrite)
                .await
            {
                Ok(data) => Reply::ok(data),
                Err(e) => Reply::err_msg(client_code(&e), e.message.clone()),
            }
        }
        None => {
            let (id, mut doc) = match envelope::unwrap(envelope_value) {
                Ok(pair) => pair,
                Err(e) => return Reply::err_msg("bad_request", e.message()),
            };
            if !overwrite && conv_store::conversation_exists(&id) {
                return Reply::err_msg(
                    "already_exists",
                    format!(
                        "standalone conversation '{id}' already exists — pass overwrite:true to replace it"
                    ),
                );
            }
            // Standalone documents carry an empty workspace binding.
            doc.insert("workspace_id".to_owned(), Value::String(String::new()));
            match conv_store::save_conversation(&doc) {
                Ok(()) => Reply::ok(json!({ "imported": id, "workspace_id": Value::Null })),
                Err(e) => Reply::err_msg("write_failed", e.to_string()),
            }
        }
    }
}

/// Map a client error onto a reply code: transport/breaker failures surface
/// as service_unavailable (OI-1 vocabulary); an application error keeps the
/// service's own code.
fn client_code(e: &ClientError) -> String {
    if e.transport || e.code == "breaker_open" {
        "service_unavailable".to_owned()
    } else if e.code.is_empty() {
        "service_error".to_owned()
    } else {
        e.code.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::conversations::test_support::TestEnv;
    use serde_json::json;

    // Standalone-path tests only — the workspace path is one forwarded call,
    // covered by the service-side api tests + the client policy test.

    fn seed_standalone(id: &str) {
        let doc = json!({
            "id": id, "title": "Standalone", "workspace_id": "",
            "messages": [{"role": "user", "content": "hi"}],
            "working_memory": [],
        });
        conv_store::save_conversation(doc.as_object().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn standalone_export_import_round_trips_byte_exact() {
        let _env = TestEnv::new();
        seed_standalone("exch-rt-1");

        let exported = handle_export(json!({ "conversation_id": "exch-rt-1" })).await;
        assert!(exported.ok, "{:?}", exported.error);
        let env_v = exported.data["export"].clone();
        assert_eq!(env_v["scope"], "standalone");

        // Re-import over itself (overwrite) → identical bytes on re-export.
        let landed = handle_import(json!({ "export": env_v, "overwrite": true })).await;
        assert!(landed.ok, "{:?}", landed.error);
        let again = handle_export(json!({ "conversation_id": "exch-rt-1" })).await;
        assert_eq!(
            serde_json::to_string(&exported.data["export"]).unwrap(),
            serde_json::to_string(&again.data["export"]).unwrap(),
            "standalone export → import → export must be byte-exact"
        );
    }

    #[tokio::test]
    async fn standalone_collision_requires_overwrite() {
        let _env = TestEnv::new();
        seed_standalone("exch-coll-1");
        let env_v = envelope::wrap(
            "workspace", // scope label is provenance only — imports anywhere
            json!({"id": "exch-coll-1", "title": "Incoming", "messages": []}),
        );
        let refused = handle_import(json!({ "export": env_v })).await;
        assert_eq!(refused.error.unwrap().code, "already_exists");
        let forced = handle_import(json!({ "export": env_v, "overwrite": true })).await;
        assert!(forced.ok);
        let doc = conv_store::read_conversation("exch-coll-1").unwrap();
        assert_eq!(doc["title"], "Incoming");
        assert_eq!(doc["workspace_id"], "", "standalone binding cleared");
    }

    #[tokio::test]
    async fn validation_errors_are_distinct() {
        let _env = TestEnv::new();
        let r = handle_export(json!({})).await;
        assert_eq!(r.error.unwrap().code, "bad_request");
        let r = handle_export(json!({ "conversation_id": "no-such-conv-xyz" })).await;
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = handle_import(json!({})).await;
        assert_eq!(r.error.unwrap().code, "bad_request");
        let r = handle_import(json!({ "export": {"format": "junk"} })).await;
        assert_eq!(r.error.unwrap().code, "bad_request");
    }
}
