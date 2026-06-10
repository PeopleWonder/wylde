//! `time` resource — the time-utility verb migration (consolidation
//! Slice 4b, `docs/plans/tool-registry-consolidation.md` §6 follow-up).
//! Folds the two `time.*` named tools into one singleton resource:
//!
//! | Verb call | Delegates to |
//! |---|---|
//! | `wylde_get("time")` | [`tools_time::run_time_now`] (current time, no id) |
//! | `wylde_execute("time", "format", {params:{epoch_ms, tz?}})` | [`tools_time::run_time_format`] |
//!
//! ## Shape — a singleton + a formatting action
//!
//! "The current time" is a singleton with no instance identity, so it is
//! a plain `get` (the `rag_graph_stats` precedent). Formatting an
//! arbitrary epoch is a pure transform, not a fetch of "the time", so it
//! is an `execute` action — `wylde_execute("time", "format", …)`. Both
//! ops are read-only (no side effects): `destructive_ops` is empty.
//!
//! ## Adapter pattern — no logic duplication
//!
//! Each [`OpHandler`] reshapes its [`ResourceRequest`] into the `args`
//! object the existing `time.*` primitive accepts, then calls straight
//! through. The named tools stay registered and unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;
use crate::tooling::tools::time_tools as tools_time;

/// Register the `time` resource into the built-in registry.
pub fn register_time_resource(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Get, op_handler(time_get));
    operations.insert(ResourceOp::Execute, op_handler(time_execute));

    reg.register_builtin(ResourceDefinition {
        resource_type: "time",
        display_name: "Time",
        description: "System clock utilities. get (current UTC/local/epoch_ms — a singleton, \
                      no id); execute action='format' (render an epoch_ms as ISO-8601 in utc/local).",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &[],
        operations,
        // Both ops are pure reads/transforms — nothing to gate.
        destructive_ops: &[],
        describe: describe_value(describe_time),
    });
}

/// `wylde_get("time")` → current time. Singleton — no id needed.
async fn time_get(
    _req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> Result<Value, IpcError> {
    tools_time::run_time_now().await
}

/// `wylde_execute("time", "format", {params:{epoch_ms, tz?}})` →
/// [`tools_time::run_time_format`]. Only the `format` action exists.
fn time_execute(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let action = req.action.clone().unwrap_or_default();
    let args = as_object(req.params);
    async move {
        match action.as_str() {
            "format" => tools_time::run_time_format(Value::Object(args)).await,
            "" => Ok(json!({
                "status": "error",
                "error": "wylde_execute(\"time\", …) requires an 'action' of \"format\"",
                "known_actions": ["format"],
            })),
            other => Ok(json!({
                "status": "error",
                "error": format!("unknown time action {other:?}; expected \"format\""),
                "known_actions": ["format"],
            })),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn as_object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

// ── describe() ───────────────────────────────────────────────────────

fn describe_time() -> Value {
    json!({
        "resource_type": "time",
        "display_name": "Time",
        "description": "System clock utilities (singleton).",
        "scope": "global",
        "identifier_fields": [],
        "filter_fields": [],
        "operations": {
            "get": {
                "verb": "wylde_get",
                "destructive": false,
                "description": "Current time: UTC ISO-8601, local ISO-8601, and epoch_ms. No id required.",
                "schema": {"type": "object", "properties": {}}
            },
            "execute": {
                "verb": "wylde_execute",
                "destructive": false,
                "description": "Format a Unix epoch-millisecond timestamp as ISO-8601.",
                "actions": [
                    {"name": "format", "description": "Render epoch_ms as ISO-8601 in the chosen zone"}
                ],
                "schema": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["format"]},
                        "params": {
                            "type": "object",
                            "properties": {
                                "epoch_ms": {"type": "number", "description": "Unix epoch milliseconds"},
                                "tz": {"type": "string", "description": "'utc' (default) or 'local'"}
                            },
                            "required": ["epoch_ms"]
                        }
                    },
                    "required": ["action"]
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> &'static crate::config::Config {
        Box::leak(Box::new(crate::config::Config::default_for_tests()))
    }

    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_time_resource(&mut r);
        r
    }

    async fn dispatch(op: ResourceOp, req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup("time").expect("time registered");
        let handler = def.operations.get(&op).expect("op registered").clone();
        let ctx = ToolContext::for_op("time", op, req.resource_id.clone());
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[test]
    fn registers_time_resource() {
        assert!(reg().lookup("time").is_some());
    }

    #[test]
    fn supports_get_and_execute_only() {
        let r = reg();
        let def = r.lookup("time").unwrap();
        assert_eq!(
            def.supported_ops(),
            vec![ResourceOp::Get, ResourceOp::Execute]
        );
    }

    #[test]
    fn nothing_is_destructive() {
        let r = reg();
        let def = r.lookup("time").unwrap();
        assert!(!def.is_destructive(ResourceOp::Get));
        assert!(!def.is_destructive(ResourceOp::Execute));
    }

    #[tokio::test]
    async fn get_returns_current_time() {
        let out = dispatch(ResourceOp::Get, ResourceRequest::default()).await;
        assert_eq!(out["status"], "success");
        assert!(out["utc"].as_str().unwrap().contains('T'));
        assert!(out["epoch_ms"].as_i64().unwrap() > 1_700_000_000_000);
    }

    #[tokio::test]
    async fn execute_format_round_trips() {
        // 2026-01-01T00:00:00.000Z = 1_767_225_600_000
        let out = dispatch(
            ResourceOp::Execute,
            ResourceRequest {
                action: Some("format".into()),
                params: json!({"epoch_ms": 1_767_225_600_000_i64, "tz": "utc"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(out["iso"], "2026-01-01T00:00:00.000Z");
    }

    #[tokio::test]
    async fn execute_missing_action_errors_cleanly() {
        let out = dispatch(ResourceOp::Execute, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["format"]));
    }

    #[tokio::test]
    async fn execute_unknown_action_errors_cleanly() {
        let out = dispatch(
            ResourceOp::Execute,
            ResourceRequest {
                action: Some("nope".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["format"]));
    }
}
