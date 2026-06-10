//! `diff` resource — the diff-utility verb migration (consolidation
//! Slice 4b, `docs/plans/tool-registry-consolidation.md` §6 follow-up).
//! Folds the single `diff.show_diff` named tool into one action-shaped
//! resource:
//!
//! | Verb call | Delegates to |
//! |---|---|
//! | `wylde_execute("diff", "diff", {params:{a, b} \| {a_path, b_path}, …})` | [`tools_diff::run_show_diff`] |
//!
//! ## Shape — a single pure-compute action
//!
//! A diff is a pure transform of two inputs into unified-diff text — it
//! names no resource you can list/get/create. The honest shape is one
//! `execute` action (`"diff"`). It reads files when given `a_path`/
//! `b_path` but never writes, so `destructive_ops` is empty (the coarse
//! `wylde_execute` gate still applies, matching the `rag_feedback`
//! create precedent). `apply_patch` — the *mutating* counterpart — stays
//! a deferred named tool; it has no resource handler to dispatch to.
//!
//! ## Adapter pattern — no logic duplication
//!
//! The [`OpHandler`] passes the verb's `params` object straight to the
//! existing `diff.show_diff` primitive. The named tool stays registered
//! and unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;
use crate::tooling::tools::diff as tools_diff;

/// Register the `diff` resource into the built-in registry.
pub fn register_diff_resource(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Execute, op_handler(diff_execute));

    reg.register_builtin(ResourceDefinition {
        resource_type: "diff",
        display_name: "Diff",
        description: "Compute a unified diff. execute action='diff' over two strings \
                      (params.a/params.b) or two files (params.a_path/params.b_path). \
                      Read-only — returns the diff text and a `changed` flag.",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &[],
        operations,
        // Pure compute / reads only — no mutation to gate.
        destructive_ops: &[],
        describe: describe_value(describe_diff),
    });
}

/// `wylde_execute("diff", "diff", {params:{a, b} | {a_path, b_path}, …})`
/// → [`tools_diff::run_show_diff`]. Only the `diff` action exists.
fn diff_execute(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let action = req.action.clone().unwrap_or_default();
    let args = as_object(req.params);
    async move {
        match action.as_str() {
            "diff" => tools_diff::run_show_diff(Value::Object(args)).await,
            "" => Ok(json!({
                "status": "error",
                "error": "wylde_execute(\"diff\", …) requires an 'action' of \"diff\"",
                "known_actions": ["diff"],
            })),
            other => Ok(json!({
                "status": "error",
                "error": format!("unknown diff action {other:?}; expected \"diff\""),
                "known_actions": ["diff"],
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

fn describe_diff() -> Value {
    json!({
        "resource_type": "diff",
        "display_name": "Diff",
        "description": "Compute a unified diff between two strings or two files.",
        "scope": "global",
        "identifier_fields": [],
        "filter_fields": [],
        "operations": {
            "execute": {
                "verb": "wylde_execute",
                "destructive": false,
                "description": "Unified diff. Provide params.a + params.b (strings) OR \
                                params.a_path + params.b_path (files).",
                "actions": [
                    {"name": "diff", "description": "Generate a unified diff; returns {diff, lines, changed}"}
                ],
                "schema": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["diff"]},
                        "params": {
                            "type": "object",
                            "properties": {
                                "a": {"type": "string", "description": "Content A (string mode)"},
                                "b": {"type": "string", "description": "Content B (string mode)"},
                                "a_path": {"type": "string", "description": "Path to file A (file mode)"},
                                "b_path": {"type": "string", "description": "Path to file B (file mode)"},
                                "a_label": {"type": "string", "description": "Header label for A (string mode; default 'a')"},
                                "b_label": {"type": "string", "description": "Header label for B (string mode; default 'b')"},
                                "context": {"type": "number", "description": "Lines of context (default 3)"}
                            }
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
        register_diff_resource(&mut r);
        r
    }

    async fn dispatch(req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup("diff").expect("diff registered");
        let handler = def
            .operations
            .get(&ResourceOp::Execute)
            .expect("execute registered")
            .clone();
        let ctx = ToolContext::for_op("diff", ResourceOp::Execute, None);
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[test]
    fn registers_diff_resource() {
        assert!(reg().lookup("diff").is_some());
    }

    #[test]
    fn supports_execute_only_and_is_not_destructive() {
        let r = reg();
        let def = r.lookup("diff").unwrap();
        assert_eq!(def.supported_ops(), vec![ResourceOp::Execute]);
        assert!(!def.is_destructive(ResourceOp::Execute));
    }

    #[tokio::test]
    async fn execute_diff_emits_inserts_and_deletes() {
        let out = dispatch(ResourceRequest {
            action: Some("diff".into()),
            params: json!({"a": "line1\nline2\n", "b": "line1\nline3\n"}),
            ..Default::default()
        })
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(out["changed"], true);
        let diff = out["diff"].as_str().unwrap();
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+line3"));
    }

    #[tokio::test]
    async fn execute_diff_unchanged_when_equal() {
        let out = dispatch(ResourceRequest {
            action: Some("diff".into()),
            params: json!({"a": "same", "b": "same"}),
            ..Default::default()
        })
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(out["changed"], false);
    }

    #[tokio::test]
    async fn execute_missing_action_errors_cleanly() {
        let out = dispatch(ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["diff"]));
    }

    #[tokio::test]
    async fn execute_unknown_action_errors_cleanly() {
        let out = dispatch(ResourceRequest {
            action: Some("apply".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["diff"]));
    }

    #[tokio::test]
    async fn execute_diff_missing_inputs_errors_cleanly() {
        // action present but no a/b or a_path/b_path → the underlying
        // primitive surfaces its own clean error.
        let out = dispatch(ResourceRequest {
            action: Some("diff".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(out["status"], "error");
    }
}
