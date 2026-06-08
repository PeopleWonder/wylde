//! `api.rs` — the harness's **compat-shim proxy** for the `workspaces.*`
//! verbs (Thought Bubble System, Slice 0b).
//!
//! **Conceptual path:** `Core/Harness/Workspaces/api`.
//!
//! The workspace registry / persona / RAG logic moved to the new
//! `wylde-workspaces` service crate. The harness pipe still answers the same
//! `workspaces.*` verbs — consumers (the GUI, the CLI) are pinned to it until
//! Slice 0d repoints them — but each handler here is now a thin forwarder:
//!
//! 1. **Forward** the request to the running `wylde-workspaces` service over
//!    its pipe via the `wylde-workspaces-client` crate.
//! 2. **Fall back** to in-process execution (through the `wylde_workspaces`
//!    lib — the *same* code the service runs) when the service isn't
//!    reachable (pipe missing / timeout / breaker open). This is the
//!    "call both pipes" compat shim from the build order: the old pipe keeps
//!    working whether or not the new service has been launched yet.
//! 3. A genuine **application error** from the service (e.g. `not_found`,
//!    `bad_request`) is authoritative — it's surfaced as-is, *not* re-run
//!    locally (the service and the in-process path read the same data dir, so
//!    re-running would only duplicate work and could race the live writer).
//!
//! Single source of truth = `wylde_workspaces`. Slice 0d removes this shim
//! and repoints consumers straight at the service pipe.

use serde_json::Value;
use wylde_shared::ipc::Reply;
use wylde_workspaces::api as local;
use wylde_workspaces_client::{ClientError, WorkspacesClient};

/// Bare service name the shim forwards to. Defaults to `wylde-workspaces`;
/// overridable via `WYLDE_HARNESS_WORKSPACES_SERVICE` so tests can point the
/// shim at a guaranteed-dead pipe and exercise the in-process fallback
/// deterministically (and never touch a real running service / its data).
fn workspaces_service() -> String {
    std::env::var("WYLDE_HARNESS_WORKSPACES_SERVICE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wylde-workspaces".to_owned())
}

fn client() -> WorkspacesClient {
    WorkspacesClient::for_service(workspaces_service())
}

/// True when the client error means the service did NOT give an authoritative
/// answer — unreachable (transport) or the breaker is open. Those fall back
/// in-process; everything else is a real service answer and is surfaced.
fn should_fall_back(e: &ClientError) -> bool {
    e.transport || e.code == "breaker_open"
}

/// Forward `verb`+`payload` to the service. `Ok(reply)` is the decision
/// (service answer or surfaced app error); `Err(())` signals "service
/// unreachable — run the local fallback".
async fn forward(verb: &str, payload: Value) -> Result<Reply, ()> {
    match client().call_verb(verb, payload, 1).await {
        Ok(data) => Ok(Reply::ok(data)),
        Err(e) if should_fall_back(&e) => Err(()),
        Err(e) => Ok(Reply::err_msg(e.code, e.message)),
    }
}

/// `workspaces.set_active`.
pub async fn handle_set_active(payload: Value) -> Reply {
    match forward("workspaces.set_active", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_set_active(payload).await,
    }
}

/// `workspaces.create`.
pub async fn handle_create(payload: Value) -> Reply {
    match forward("workspaces.create", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_create(payload).await,
    }
}

/// `workspaces.update`.
pub async fn handle_update(payload: Value) -> Reply {
    match forward("workspaces.update", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_update(payload).await,
    }
}

/// `workspaces.delete`.
pub async fn handle_delete(payload: Value) -> Reply {
    match forward("workspaces.delete", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_delete(payload).await,
    }
}

/// `workspaces.set_persona`.
pub async fn handle_set_persona(payload: Value) -> Reply {
    match forward("workspaces.set_persona", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_set_persona(payload).await,
    }
}

/// `workspaces.list_mru`.
pub async fn handle_list_mru(payload: Value) -> Reply {
    match forward("workspaces.list_mru", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_list_mru(payload).await,
    }
}

/// `workspaces.rag_query`.
pub async fn handle_rag_query(payload: Value) -> Reply {
    match forward("workspaces.rag_query", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_rag_query(payload).await,
    }
}

/// `workspaces.reindex`.
pub async fn handle_reindex(payload: Value) -> Reply {
    match forward("workspaces.reindex", payload.clone()).await {
        Ok(reply) => reply,
        Err(()) => local::handle_reindex(payload).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;
    use serde_json::json;
    use tempfile::tempdir;

    // `TestEnv` points the shim at a dead pipe, so every call here falls
    // back in-process (through the `wylde_workspaces` lib) against the test's
    // isolated `WYLDE_DATA_DIR` — deterministic regardless of whether a real
    // workspaces service happens to be running.

    #[tokio::test]
    async fn create_then_list_falls_back_in_process() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let folder = td.path().join("proj");
        std::fs::create_dir(&folder).unwrap();

        let created = handle_create(json!({ "folder": folder.to_string_lossy(), "name": "P" })).await;
        assert!(created.ok, "create (fallback) failed: {:?}", created.error);
        let id = created.data["id"].as_str().unwrap().to_owned();

        let listed = handle_list_mru(Value::Null).await;
        assert!(listed.ok);
        assert_eq!(listed.data["active_id"], id);
        assert_eq!(listed.data["workspaces"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_bad_request_falls_back_and_surfaces_error() {
        let _env = TestEnv::new();
        // No folder → the in-process fallback rejects it with bad_request.
        let reply = handle_create(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }
}
