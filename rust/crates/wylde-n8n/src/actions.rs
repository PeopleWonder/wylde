//! `n8n.*` action handlers — thin payload adapters over [`crate::client`].
//!
//! Each handler decodes the pipe payload, delegates to one client verb,
//! and wraps the resulting envelope in `Reply::ok`. The envelope itself
//! may be an n8n-level error dict (`{"error": …, "detail"/"code": …}`)
//! — that is DATA, not a pipe failure, exactly as the Python tools
//! propagated the client's error dicts into the tool-runner envelope.
//! `Reply::err` is reserved for payload-schema problems the Python
//! layer also rejected locally (and those, too, mostly surface as the
//! client's own in-envelope guards for byte-level parity).
//!
//! Each handler accepts a client handle so tests can point at a
//! wiremock server (the wylde-ollama `with_upstream` pattern).

use std::sync::{Arc, OnceLock};

use serde_json::Value;
use wylde_shared::ipc::Reply;

use crate::client::N8nClient;
use crate::config::Config;

/// Process-wide shared client. Built lazily on first access from the
/// env-derived [`Config`].
pub fn client() -> Arc<N8nClient> {
    static CLIENT: OnceLock<Arc<N8nClient>> = OnceLock::new();
    CLIENT
        .get_or_init(|| Arc::new(N8nClient::new(Config::get().auth.clone())))
        .clone()
}

/// Pull an id field out of the payload, coercing a JSON number to its
/// string form (LLM callers routinely pass `workflow_id: 5`; Python's
/// `str(workflow_id)` accepted that, so the pipe surface does too).
fn id_field(payload: &Value, field: &str) -> String {
    match payload.get(field) {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// `n8n.health {}` → `{auth_configured, url, reachable}`.
pub async fn handle_health(_payload: Value, client: Arc<N8nClient>) -> Reply {
    Reply::ok(client.health().await)
}

/// `n8n.list_workflows {}` → `{workflows: [{id,name,active,description}], count}`.
pub async fn handle_list_workflows(_payload: Value, client: Arc<N8nClient>) -> Reply {
    Reply::ok(client.list_workflows().await)
}

/// `n8n.get_workflow {workflow_id}` → `{workflow}` or an error envelope.
pub async fn handle_get_workflow(payload: Value, client: Arc<N8nClient>) -> Reply {
    let id = id_field(&payload, "workflow_id");
    Reply::ok(client.get_workflow(&id).await)
}

/// `n8n.get_execution {execution_id}` → `{execution}` or an error envelope.
pub async fn handle_get_execution(payload: Value, client: Arc<N8nClient>) -> Reply {
    let id = id_field(&payload, "execution_id");
    Reply::ok(client.get_execution(&id).await)
}

/// `n8n.execute_workflow {workflow_id, inputs?}` →
/// `{execution_id, status, data}` or an error envelope (numeric-id
/// guard preserved in the client).
pub async fn handle_execute_workflow(payload: Value, client: Arc<N8nClient>) -> Reply {
    let id = id_field(&payload, "workflow_id");
    let inputs = payload.get("inputs").cloned().filter(|v| !v.is_null());
    Reply::ok(client.execute_workflow(&id, inputs).await)
}

/// `n8n.create_workflow {name, nodes?, connections?, active?, settings?}`
/// → `{workflow_id, name, active, created_at}` or an error envelope.
pub async fn handle_create_workflow(payload: Value, client: Arc<N8nClient>) -> Reply {
    Reply::ok(client.create_workflow(&payload).await)
}

/// `n8n.edit_workflow {workflow_id, name?/nodes?/connections?/active?}`
/// → `{workflow_id, name, active, updated_at}`. PATCHes only provided
/// keys; the "No updatable fields provided" guard lives in the client.
pub async fn handle_edit_workflow(payload: Value, client: Arc<N8nClient>) -> Reply {
    let id = id_field(&payload, "workflow_id");
    Reply::ok(client.edit_workflow(&id, &payload).await)
}

/// `n8n.delete_workflow {workflow_id}` → `{deleted: true, workflow_id}`.
/// Archive-then-delete sequence preserved in the client.
pub async fn handle_delete_workflow(payload: Value, client: Arc<N8nClient>) -> Reply {
    let id = id_field(&payload, "workflow_id");
    Reply::ok(client.delete_workflow(&id).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::config::AuthConfig;

    /// A client that fails fast (no creds) — handlers must surface the
    /// structured envelope inside an ok Reply, never a pipe error.
    fn unconfigured() -> Arc<N8nClient> {
        Arc::new(N8nClient::new(AuthConfig {
            url: "http://127.0.0.1:1".into(),
            ..Default::default()
        }))
    }

    #[test]
    fn id_field_coerces_numbers_like_python_str() {
        assert_eq!(id_field(&json!({"workflow_id": "7"}), "workflow_id"), "7");
        assert_eq!(id_field(&json!({"workflow_id": 7}), "workflow_id"), "7");
        assert_eq!(id_field(&json!({}), "workflow_id"), "");
        assert_eq!(id_field(&json!({"workflow_id": null}), "workflow_id"), "");
    }

    #[tokio::test]
    async fn handlers_wrap_auth_envelope_in_ok_reply() {
        let c = unconfigured();
        for reply in [
            handle_list_workflows(json!({}), c.clone()).await,
            handle_get_workflow(json!({"workflow_id": "1"}), c.clone()).await,
            handle_get_execution(json!({"execution_id": "1"}), c.clone()).await,
            handle_execute_workflow(json!({"workflow_id": "1"}), c.clone()).await,
            handle_create_workflow(json!({"name": "x"}), c.clone()).await,
            handle_edit_workflow(json!({"workflow_id": "1", "name": "y"}), c.clone()).await,
            handle_delete_workflow(json!({"workflow_id": "1"}), c.clone()).await,
        ] {
            assert!(reply.ok, "envelope errors are data, not pipe errors");
            assert_eq!(reply.data["code"], "auth_not_configured");
        }
    }

    #[tokio::test]
    async fn health_never_auth_gates() {
        let reply = handle_health(json!({}), unconfigured()).await;
        assert!(reply.ok);
        assert_eq!(reply.data["auth_configured"], false);
        assert_eq!(reply.data["reachable"], false);
        assert_eq!(reply.data["url"], "http://127.0.0.1:1");
    }

    #[tokio::test]
    async fn missing_ids_surface_the_python_guard_messages() {
        // An api-key client on an un-dialled URL: every guard below
        // (id-required / numeric / no-fields / name-required) answers
        // BEFORE any request is attempted, so the dead URL proves the
        // checks are local.
        let c2 = Arc::new(N8nClient::new(AuthConfig {
            url: "http://127.0.0.1:1".into(),
            api_key: "k".into(),
            ..Default::default()
        }));
        let reply = handle_get_workflow(json!({}), c2.clone()).await;
        assert_eq!(reply.data["error"], "workflow_id is required");
        let reply = handle_get_execution(json!({}), c2.clone()).await;
        assert_eq!(reply.data["error"], "execution_id is required");
        let reply = handle_execute_workflow(json!({"workflow_id": "abc"}), c2.clone()).await;
        assert_eq!(reply.data["error"], "workflow_id must be a numeric string");
        let reply = handle_edit_workflow(json!({"workflow_id": "1"}), c2.clone()).await;
        assert_eq!(reply.data["error"], "No updatable fields provided");
        let reply = handle_create_workflow(json!({}), c2).await;
        assert_eq!(reply.data["error"], "name is required");
    }
}
