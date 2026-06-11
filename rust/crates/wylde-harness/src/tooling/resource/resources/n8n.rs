//! N8N resources (taxonomy reorg TX S3) — LLM access to n8n workflows
//! restored on the verb layer, replacing the dead Python
//! `N8N/tools/n8n_*` named tools:
//!
//! | Resource | Op | Service action | Python predecessor |
//! |---|---|---|---|
//! | `n8n_workflow` | list | `n8n.list_workflows` | n8n_list_workflows |
//! | `n8n_workflow` | get | `n8n.get_workflow` | n8n_get_workflow |
//! | `n8n_workflow` | execute (`run`) | `n8n.execute_workflow` | n8n_execute_workflow |
//! | `n8n_workflow` | create | `n8n.create_workflow` | n8n_create_workflow |
//! | `n8n_workflow` | update | `n8n.edit_workflow` | n8n_edit_workflow |
//! | `n8n_workflow` | delete | `n8n.delete_workflow` | n8n_delete_workflow |
//! | `n8n_execution` | get | `n8n.get_execution` | n8n_get_execution |
//!
//! ## Cross-process — IPC, not in-process (the treesitter pattern)
//!
//! The handlers live in the `wylde-n8n` service (pipe
//! `\\.\pipe\wylde-n8n`); each [`OpHandler`] is one
//! `ipc::call_action(cfg.n8n_service, …)` hop. **Core works without the
//! service**: an unreachable pipe folds into a structured
//! `service_unavailable` envelope — never an `Err`, never a panic, never
//! anything that blocks core — and a reachable service with no n8n
//! daemon / credentials behind it passes through the service's own
//! structured envelopes (`auth_not_configured`, transport errors).
//!
//! ## Two resources, not one with magic ids
//!
//! Execution status is a **second, get-only `n8n_execution` resource**
//! rather than an `execution:<id>` prefix convention on `n8n_workflow`'s
//! `get`: a workflow and an execution are genuinely different resource
//! identities (different n8n endpoints, different id spaces), and the
//! resource layer registers sibling definitions trivially (the
//! `code_chunk` / `code_entity` precedent) — cleaner than overloading
//! one `get` with a stringly-typed discriminator.
//!
//! ## Consent gating mirrors the Python `requires_confirmation` flags
//!
//! The retired tool manifests gated create/edit/delete
//! (`requires_confirmation: true`) and left list/get/execute ungated, so
//! `destructive_ops` is exactly `{create, update, delete}` — `execute`
//! deliberately stays non-destructive to preserve the shipped gating
//! surface byte-for-byte.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::{self, IpcError};

use crate::config::Config;
use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;

/// Register the n8n resources into the built-in registry.
pub fn register_n8n_resources(reg: &mut ResourceRegistry) {
    register_n8n_workflow(reg);
    register_n8n_execution(reg);
}

// ── n8n_workflow — full CRUD + run ───────────────────────────────────

fn register_n8n_workflow(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(workflow_list));
    operations.insert(ResourceOp::Get, op_handler(workflow_get));
    operations.insert(ResourceOp::Create, op_handler(workflow_create));
    operations.insert(ResourceOp::Update, op_handler(workflow_update));
    operations.insert(ResourceOp::Delete, op_handler(workflow_delete));
    operations.insert(ResourceOp::Execute, op_handler(workflow_execute));

    reg.register_builtin(ResourceDefinition {
        resource_type: "n8n_workflow",
        display_name: "n8n workflow",
        description: "A workflow in the external n8n automation engine (via the optional \
                      wylde-n8n service). list/get to inspect, create/update/delete to author \
                      (consent-gated), execute action='run' to trigger a run. Degrades to a \
                      structured service_unavailable error when the service is down.",
        scope: Scope::Global,
        identifier_fields: &["workflow_id"],
        filter_fields: &[],
        operations,
        // Mirrors the Python tools' requires_confirmation flags:
        // create/edit/delete were gated; list/get/execute were not.
        destructive_ops: &[ResourceOp::Create, ResourceOp::Update, ResourceOp::Delete],
        describe: describe_value(describe_n8n_workflow),
    });
}

/// `wylde_list("n8n_workflow")` → `n8n.list_workflows`. No arguments.
async fn workflow_list(
    _req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> Result<Value, IpcError> {
    call_n8n(cfg, "n8n.list_workflows", "n8n_workflow", json!({})).await
}

/// `wylde_get("n8n_workflow", <id>)` → `n8n.get_workflow`.
fn workflow_get(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let id = workflow_id_of(&req);
    async move {
        let Some(id) = id else {
            return Ok(missing(
                "wylde_get(\"n8n_workflow\", …) requires 'resource_id' (the workflow id)",
            ));
        };
        call_n8n(
            cfg,
            "n8n.get_workflow",
            "n8n_workflow",
            json!({"workflow_id": id}),
        )
        .await
    }
}

/// `wylde_create("n8n_workflow", {body:{name, nodes?, connections?,
/// active?, settings?}})` → `n8n.create_workflow`. `name` is checked
/// locally so a missing name never costs an IPC hop.
fn workflow_create(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let body = as_object(req.body);
    async move {
        if body
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Ok(missing(
                "wylde_create(\"n8n_workflow\", …) requires body.name (the workflow's display name)",
            ));
        }
        call_n8n(
            cfg,
            "n8n.create_workflow",
            "n8n_workflow",
            Value::Object(body),
        )
        .await
    }
}

/// `wylde_update("n8n_workflow", <id>, {body:{name?/nodes?/connections?/
/// active?}})` → `n8n.edit_workflow` (PATCH semantics; the service keeps
/// the "No updatable fields provided" guard).
fn workflow_update(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let id = workflow_id_of(&req);
    let mut body = as_object(req.body);
    async move {
        let Some(id) = id else {
            return Ok(missing(
                "wylde_update(\"n8n_workflow\", …) requires 'resource_id' (the workflow id)",
            ));
        };
        body.insert("workflow_id".into(), json!(id));
        call_n8n(
            cfg,
            "n8n.edit_workflow",
            "n8n_workflow",
            Value::Object(body),
        )
        .await
    }
}

/// `wylde_delete("n8n_workflow", <id>)` → `n8n.delete_workflow`
/// (archive-then-delete on the service side).
fn workflow_delete(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let id = workflow_id_of(&req);
    async move {
        let Some(id) = id else {
            return Ok(missing(
                "wylde_delete(\"n8n_workflow\", …) requires 'resource_id' (the workflow id)",
            ));
        };
        call_n8n(
            cfg,
            "n8n.delete_workflow",
            "n8n_workflow",
            json!({"workflow_id": id}),
        )
        .await
    }
}

/// `wylde_execute("n8n_workflow", "run", {params:{workflow_id?, inputs?}})`
/// → `n8n.execute_workflow`. The id may ride as `resource_id` or
/// `params.workflow_id`; the numeric-id guard stays in the service.
fn workflow_execute(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let action = req.action.clone().unwrap_or_default();
    let params = as_object(req.params);
    let id = req
        .resource_id
        .clone()
        .or_else(|| params.get("workflow_id").map(id_string));
    async move {
        match action.as_str() {
            "run" => {
                let Some(id) = id else {
                    return Ok(missing(
                        "wylde_execute(\"n8n_workflow\", \"run\", …) requires a workflow id \
                         (resource_id or params.workflow_id)",
                    ));
                };
                let mut payload = Map::new();
                payload.insert("workflow_id".into(), json!(id));
                if let Some(inputs) = params.get("inputs").filter(|v| !v.is_null()) {
                    payload.insert("inputs".into(), inputs.clone());
                }
                call_n8n(
                    cfg,
                    "n8n.execute_workflow",
                    "n8n_workflow",
                    Value::Object(payload),
                )
                .await
            }
            "" => Ok(json!({
                "status": "error",
                "error": "wylde_execute(\"n8n_workflow\", …) requires an 'action' of \"run\"",
                "known_actions": ["run"],
            })),
            other => Ok(json!({
                "status": "error",
                "error": format!("unknown n8n_workflow action {other:?}; expected \"run\""),
                "known_actions": ["run"],
            })),
        }
    }
}

// ── n8n_execution — get-only execution status ────────────────────────

fn register_n8n_execution(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Get, op_handler(execution_get));

    reg.register_builtin(ResourceDefinition {
        resource_type: "n8n_execution",
        display_name: "n8n execution",
        description: "One run of an n8n workflow — get by execution id (as returned by \
                      executing an n8n_workflow) for status + output data. Read-only.",
        scope: Scope::Global,
        identifier_fields: &["execution_id"],
        filter_fields: &[],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_n8n_execution),
    });
}

/// `wylde_get("n8n_execution", <id>)` → `n8n.get_execution`.
fn execution_get(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let id = req
        .resource_id
        .clone()
        .or_else(|| req.body.get("execution_id").map(id_string))
        .filter(|s| !s.is_empty());
    async move {
        let Some(id) = id else {
            return Ok(missing(
                "wylde_get(\"n8n_execution\", …) requires 'resource_id' (the execution id)",
            ));
        };
        call_n8n(
            cfg,
            "n8n.get_execution",
            "n8n_execution",
            json!({"execution_id": id}),
        )
        .await
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// One `ipc::call_action` hop to the wylde-n8n service. A failed call —
/// including the service being down entirely — folds into a clean
/// `status: "error"` / `code: "service_unavailable"` envelope rather
/// than propagating as a hard `Err`, so a missing service never aborts
/// the turn (core works WITHOUT n8n). Successful replies carry the
/// service's Python-parity envelopes: an `{"error": …}` object is an
/// n8n-level failure and is stamped `status: "error"` (its own `code`,
/// e.g. `auth_not_configured` / `not_found`, wins); anything else is
/// stamped `status: "ok"`.
async fn call_n8n(
    cfg: &Config,
    action: &str,
    resource_type: &str,
    payload: Value,
) -> Result<Value, IpcError> {
    match ipc::call_action(&cfg.n8n_service, action, payload).await {
        Ok(v) => Ok(stamp_status(v)),
        Err(e) => Ok(json!({
            "status": "error",
            "resource_type": resource_type,
            "code": "service_unavailable",
            "error": format!("wylde-n8n service unavailable: {}: {}", e.code, e.message),
            "hint": "is the wylde-n8n service running? (pipe \\\\.\\pipe\\wylde-n8n; \
                     the n8n daemon itself is external and user-managed)",
        })),
    }
}

/// Stamp a `status` field onto a service reply (idempotent). The
/// service's error envelopes are data (`{"error": …}`), so the stamp is
/// derived from the presence of an `error` key; non-objects are wrapped.
fn stamp_status(v: Value) -> Value {
    match v {
        Value::Object(mut m) => {
            let status = if m.contains_key("error") {
                "error"
            } else {
                "ok"
            };
            m.entry("status").or_insert_with(|| json!(status));
            Value::Object(m)
        }
        other => json!({"status": "ok", "result": other}),
    }
}

/// Workflow id from `resource_id` or `body.workflow_id` (string or
/// number — n8n ids are numeric strings and models pass both).
fn workflow_id_of(req: &ResourceRequest) -> Option<String> {
    req.resource_id
        .clone()
        .or_else(|| req.body.get("workflow_id").map(id_string))
        .filter(|s| !s.is_empty())
}

fn id_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn as_object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn missing(msg: &str) -> Value {
    json!({ "status": "error", "error": msg })
}

// ── describe() ───────────────────────────────────────────────────────

fn describe_n8n_workflow() -> Value {
    json!({
        "resource_type": "n8n_workflow",
        "display_name": "n8n workflow",
        "description": "A workflow in the external n8n automation engine (optional wylde-n8n \
                        service; structured service_unavailable error when absent).",
        "scope": "global",
        "identifier_fields": ["workflow_id"],
        "filter_fields": [],
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "List workflows. Reply: {workflows: [{id, name, active, \
                                description}], count}.",
                "schema": {"type": "object", "properties": {}}
            },
            "get": {
                "verb": "wylde_get",
                "destructive": false,
                "description": "Fetch one workflow's full definition. Reply: {workflow}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Workflow id"}
                    },
                    "required": ["resource_id"]
                }
            },
            "create": {
                "verb": "wylde_create",
                "destructive": true,
                "description": "Create a workflow (inactive unless body.active=true; \
                                reversible by deleting it). Reply: {workflow_id, name, \
                                active, created_at}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string", "description": "Display name (required)"},
                                "nodes": {"type": "array", "description": "Node objects (id, name, type, typeVersion, position, parameters)"},
                                "connections": {"type": "object", "description": "Connection map keyed by source node name"},
                                "active": {"type": "boolean", "description": "Activate immediately (default false)"},
                                "settings": {"type": "object", "description": "Workflow settings"}
                            },
                            "required": ["name"]
                        }
                    }
                }
            },
            "update": {
                "verb": "wylde_update",
                "destructive": true,
                "description": "PATCH a workflow — only the provided keys of name/nodes/\
                                connections/active change. Reply: {workflow_id, name, \
                                active, updated_at}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Workflow id"},
                        "body": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "nodes": {"type": "array"},
                                "connections": {"type": "object"},
                                "active": {"type": "boolean"}
                            }
                        }
                    },
                    "required": ["resource_id"]
                }
            },
            "delete": {
                "verb": "wylde_delete",
                "destructive": true,
                "description": "Permanently delete a workflow (archived first — an n8n \
                                requirement). Reply: {deleted: true, workflow_id}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Workflow id"}
                    },
                    "required": ["resource_id"]
                }
            },
            "execute": {
                "verb": "wylde_execute",
                "destructive": false,
                "description": "Trigger a workflow run. Reply: {execution_id, status, data} \
                                — follow up with wylde_get(\"n8n_execution\", execution_id).",
                "actions": [
                    {"name": "run", "description": "Run the workflow with optional inputs"}
                ],
                "schema": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["run"]},
                        "resource_id": {"type": "string", "description": "Workflow id (numeric string)"},
                        "params": {
                            "type": "object",
                            "properties": {
                                "workflow_id": {"type": "string", "description": "Workflow id (alternative to resource_id)"},
                                "inputs": {"type": "object", "description": "Run-time input data forwarded to the workflow"}
                            }
                        }
                    },
                    "required": ["action"]
                }
            }
        }
    })
}

fn describe_n8n_execution() -> Value {
    json!({
        "resource_type": "n8n_execution",
        "display_name": "n8n execution",
        "description": "One run of an n8n workflow (read-only status surface).",
        "scope": "global",
        "identifier_fields": ["execution_id"],
        "filter_fields": [],
        "operations": {
            "get": {
                "verb": "wylde_get",
                "destructive": false,
                "description": "Fetch an execution's status payload. Reply: {execution}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Execution id (from n8n_workflow execute)"}
                    },
                    "required": ["resource_id"]
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::resource::ToolsetFilter;

    fn cfg() -> &'static Config {
        Box::leak(Box::new(Config::default_for_tests()))
    }

    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_n8n_resources(&mut r);
        r
    }

    async fn dispatch(rt: &str, op: ResourceOp, req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup(rt).expect("resource registered");
        let handler = def.operations.get(&op).expect("op registered").clone();
        let ctx = ToolContext::for_op(rt, op, req.resource_id.clone());
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[test]
    fn registers_both_n8n_resources() {
        let r = reg();
        assert!(r.lookup("n8n_workflow").is_some());
        assert!(r.lookup("n8n_execution").is_some());
        assert_eq!(r.builtin_len(), 2);
    }

    #[test]
    fn workflow_supports_full_crud_plus_execute() {
        let r = reg();
        let def = r.lookup("n8n_workflow").unwrap();
        assert_eq!(
            def.supported_ops(),
            vec![
                ResourceOp::List,
                ResourceOp::Get,
                ResourceOp::Create,
                ResourceOp::Update,
                ResourceOp::Delete,
                ResourceOp::Execute,
            ]
        );
    }

    #[test]
    fn execution_is_get_only_and_read_only() {
        let r = reg();
        let def = r.lookup("n8n_execution").unwrap();
        assert_eq!(def.supported_ops(), vec![ResourceOp::Get]);
        assert!(!def.is_destructive(ResourceOp::Get));
    }

    #[test]
    fn destructive_classification_matches_python_requires_confirmation() {
        // The retired manifests gated create/edit/delete and left
        // list/get/execute ungated — preserved exactly.
        let r = reg();
        let def = r.lookup("n8n_workflow").unwrap();
        assert!(def.is_destructive(ResourceOp::Create));
        assert!(def.is_destructive(ResourceOp::Update));
        assert!(def.is_destructive(ResourceOp::Delete));
        assert!(!def.is_destructive(ResourceOp::List));
        assert!(!def.is_destructive(ResourceOp::Get));
        assert!(!def.is_destructive(ResourceOp::Execute));
    }

    #[test]
    fn neither_resource_is_searchable() {
        let r = reg();
        assert!(r.searchable_types(&ToolsetFilter::all()).is_empty());
    }

    #[test]
    fn describe_flags_match_destructive_sets() {
        let v = describe_n8n_workflow();
        let ops = v["operations"].as_object().unwrap();
        for op in ["list", "get", "create", "update", "delete", "execute"] {
            assert!(ops.contains_key(op), "describe missing {op}");
        }
        assert_eq!(ops["create"]["destructive"], true);
        assert_eq!(ops["update"]["destructive"], true);
        assert_eq!(ops["delete"]["destructive"], true);
        assert_eq!(ops["list"]["destructive"], false);
        assert_eq!(ops["execute"]["destructive"], false);
        let actions = ops["execute"]["actions"].as_array().unwrap();
        assert_eq!(actions[0]["name"], "run");

        let v = describe_n8n_execution();
        let ops = v["operations"].as_object().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops["get"]["destructive"], false);
    }

    #[test]
    fn stamp_status_derives_error_from_envelope() {
        let ok = stamp_status(json!({"workflows": [], "count": 0}));
        assert_eq!(ok["status"], "ok");
        let err = stamp_status(json!({"error": "boom", "code": "auth_not_configured"}));
        assert_eq!(err["status"], "error");
        assert_eq!(err["code"], "auth_not_configured");
        // Idempotent + non-object wrapping.
        let pre = stamp_status(json!({"status": "ok", "x": 1}));
        assert_eq!(pre["status"], "ok");
        let wrapped = stamp_status(json!([1, 2]));
        assert_eq!(wrapped["result"], json!([1, 2]));
    }

    // ── local validation round-trips (no IPC hop) ─────────────────────

    #[tokio::test]
    async fn get_without_id_errors_locally() {
        let out = dispatch("n8n_workflow", ResourceOp::Get, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("resource_id"));
    }

    #[tokio::test]
    async fn create_without_name_errors_locally() {
        let out = dispatch(
            "n8n_workflow",
            ResourceOp::Create,
            ResourceRequest::default(),
        )
        .await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("body.name"));
    }

    #[tokio::test]
    async fn update_without_id_errors_locally() {
        let out = dispatch(
            "n8n_workflow",
            ResourceOp::Update,
            ResourceRequest {
                body: json!({"name": "renamed"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("resource_id"));
    }

    #[tokio::test]
    async fn execute_requires_the_run_action() {
        let out = dispatch(
            "n8n_workflow",
            ResourceOp::Execute,
            ResourceRequest::default(),
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["run"]));
        let out = dispatch(
            "n8n_workflow",
            ResourceOp::Execute,
            ResourceRequest {
                action: Some("bogus".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["known_actions"], json!(["run"]));
    }

    #[tokio::test]
    async fn execute_run_without_id_errors_locally() {
        let out = dispatch(
            "n8n_workflow",
            ResourceOp::Execute,
            ResourceRequest {
                action: Some("run".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("workflow id"));
    }

    #[tokio::test]
    async fn execution_get_without_id_errors_locally() {
        let out = dispatch("n8n_execution", ResourceOp::Get, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("execution id"));
    }

    // ── fail-soft IPC forwarding (no service runs in tests) ──────────

    #[tokio::test]
    async fn list_with_service_down_is_service_unavailable_not_panic() {
        // No wylde-n8n pipe exists in the unit-test env — the hop must
        // fold into the structured envelope, never an Err / panic.
        let out = dispatch("n8n_workflow", ResourceOp::List, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "service_unavailable");
        assert!(out["hint"].as_str().unwrap().contains("wylde-n8n"));
    }

    #[tokio::test]
    async fn delete_resource_id_threads_through_to_the_ipc_hop() {
        // With an id supplied, the adapter reaches the (dead) pipe and
        // surfaces service_unavailable — NOT the missing-id error.
        // Proves the id threaded past local validation.
        let out = dispatch(
            "n8n_workflow",
            ResourceOp::Delete,
            ResourceRequest {
                resource_id: Some("42".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "service_unavailable");
    }
}
