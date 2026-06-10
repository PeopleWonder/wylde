//! `model` resource — the Ollama VRAM-residency verb migration
//! (consolidation Slice 4b, `docs/plans/tool-registry-consolidation.md`
//! §6 follow-up). Folds the four `ollama.*` named tools into one
//! resource type under the generic verbs:
//!
//! | Verb call | Delegates to |
//! |---|---|
//! | `wylde_list("model")` | [`tools_ollama::run_list_loaded`] (resident set) |
//! | `wylde_create("model", {body:{model, keep_alive?}})` | [`tools_ollama::run_preload`] (load into VRAM) |
//! | `wylde_delete("model", model)` | [`tools_ollama::run_evict`] (release from VRAM) |
//! | `wylde_execute("model", "auto_evict_lru", {params:{threshold_mb?, dry_run?}})` | [`tools_ollama::run_auto_evict_lru`] (LRU sweep) |
//!
//! ## Why CRUD, not all-execute (the thoughtful shape)
//!
//! A *loaded model* has a genuine resource identity — the Ollama model
//! tag — and VRAM residency is the thing the verbs operate on:
//!
//! * `preload` **creates** a residency (loads the tag into VRAM),
//! * `evict` **deletes** that residency (unloads the tag),
//! * `list_loaded` **enumerates** the resident set.
//!
//! That maps cleanly onto `create` / `delete` / `list` — `wylde_delete
//! ("model", "qwen2.5:7b")` is the natural way to evict. Only
//! `auto_evict_lru` resists CRUD: it is a *bulk sweep* over the whole
//! resident set with no single-instance identity, so it stays an
//! `execute` action (the `rag` index-trigger precedent).
//!
//! ## Not the `models.*` registry cluster
//!
//! These four tools manage *VRAM residency* via the `wylde-ollama`
//! service (`ollama.list_loaded` / `preload` / `eject`). They are a
//! **different cluster** from the Slice-3a `models.*` model-registry pipe
//! verbs (`crate::model_registry`), which describe routing/profile
//! metadata and were never registered as a verb resource. No duplicate
//! registration — this resource is `model`; the registry surface is
//! untouched.
//!
//! ## Adapter pattern — no logic duplication (the memory.rs template)
//!
//! Each [`OpHandler`] reshapes its [`ResourceRequest`] into the `args`
//! object an existing `ollama.*` primitive accepts, then calls straight
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
use crate::tooling::tools::ollama as tools_ollama;

/// Register the `model` resource into the built-in registry.
pub fn register_model_resource(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(model_list));
    operations.insert(ResourceOp::Create, op_handler(model_create));
    operations.insert(ResourceOp::Delete, op_handler(model_delete));
    operations.insert(ResourceOp::Execute, op_handler(model_execute));

    reg.register_builtin(ResourceDefinition {
        resource_type: "model",
        display_name: "Loaded model (VRAM residency)",
        description: "An Ollama model's VRAM residency. list (models resident now), \
                      create (preload a tag into VRAM), delete (evict a tag), execute \
                      action='auto_evict_lru' (sweep LRU until VRAM drops below a threshold).",
        scope: Scope::Global,
        identifier_fields: &["model"],
        filter_fields: &[],
        operations,
        // preload / evict / sweep all mutate VRAM state. list reads only.
        destructive_ops: &[ResourceOp::Create, ResourceOp::Delete, ResourceOp::Execute],
        describe: describe_value(describe_model),
    });
}

/// `wylde_list("model")` → resident set. No arguments.
async fn model_list(
    _req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> Result<Value, IpcError> {
    tools_ollama::run_list_loaded(Value::Null).await
}

/// `wylde_create("model", {body:{model, keep_alive?}})` → preload the tag
/// into VRAM. The tag may also arrive as `resource_id`.
fn model_create(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.body);
    if let Some(id) = req.resource_id {
        args.entry("model").or_insert(json!(id));
    }
    async move {
        if args.get("model").and_then(Value::as_str).is_none() {
            return Ok(missing(
                "wylde_create(\"model\", …) requires body.model (the tag to preload)",
            ));
        }
        tools_ollama::run_preload(Value::Object(args)).await
    }
}

/// `wylde_delete("model", model)` → evict the tag from VRAM. The tag is
/// the verb's `resource_id` (or `body.model`).
fn model_delete(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let model = req
        .resource_id
        .clone()
        .or_else(|| str_field(&req.body, "model"));
    async move {
        match model {
            Some(m) => tools_ollama::run_evict(json!({ "model": m })).await,
            None => Ok(missing(
                "wylde_delete(\"model\", …) requires 'resource_id' (the model tag)",
            )),
        }
    }
}

/// `wylde_execute("model", "auto_evict_lru", {params:{threshold_mb?,
/// dry_run?}})` → LRU sweep. Only one action exists; an unknown / missing
/// action returns a clean error listing it.
fn model_execute(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let action = req.action.clone().unwrap_or_default();
    let args = as_object(req.params);
    async move {
        match action.as_str() {
            "auto_evict_lru" => tools_ollama::run_auto_evict_lru(Value::Object(args)).await,
            "" => Ok(json!({
                "status": "error",
                "error": "wylde_execute(\"model\", …) requires an 'action' of \"auto_evict_lru\"",
                "known_actions": ["auto_evict_lru"],
            })),
            other => Ok(json!({
                "status": "error",
                "error": format!("unknown model action {other:?}; expected \"auto_evict_lru\""),
                "known_actions": ["auto_evict_lru"],
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

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn missing(msg: &str) -> Value {
    json!({ "status": "error", "error": msg })
}

// ── describe() ───────────────────────────────────────────────────────

fn describe_model() -> Value {
    json!({
        "resource_type": "model",
        "display_name": "Loaded model (VRAM residency)",
        "description": "An Ollama model's VRAM residency (via wylde-ollama). \
                        Distinct from the models.* model-registry surface.",
        "scope": "global",
        "identifier_fields": ["model"],
        "filter_fields": [],
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "List models resident in VRAM now (name, size, size_vram, expires_at).",
                "schema": {"type": "object", "properties": {}}
            },
            "create": {
                "verb": "wylde_create",
                "destructive": true,
                "description": "Preload a model tag into VRAM without generating tokens. \
                                Idempotent — refreshes keep_alive if already resident.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "model": {"type": "string", "description": "Ollama model tag, e.g. 'qwen2.5:7b'"},
                                "keep_alive": {"type": "string", "description": "Resident TTL (duration string or seconds; default '24h')"}
                            },
                            "required": ["model"]
                        }
                    }
                }
            },
            "delete": {
                "verb": "wylde_delete",
                "destructive": true,
                "description": "Evict a model tag from VRAM (keep_alive=0).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Model tag to evict"}
                    },
                    "required": ["resource_id"]
                }
            },
            "execute": {
                "verb": "wylde_execute",
                "destructive": true,
                "description": "Bulk VRAM management with no single-tag identity.",
                "actions": [
                    {"name": "auto_evict_lru", "description": "Evict soonest-to-expire models until VRAM < threshold_mb"}
                ],
                "schema": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["auto_evict_lru"]},
                        "params": {
                            "type": "object",
                            "properties": {
                                "threshold_mb": {"type": "number", "description": "VRAM target in MiB (default 20000)"},
                                "dry_run": {"type": "boolean", "description": "Compute the plan without evicting"}
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
    use crate::tooling::resource::ToolsetFilter;

    fn cfg() -> &'static crate::config::Config {
        Box::leak(Box::new(crate::config::Config::default_for_tests()))
    }

    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_model_resource(&mut r);
        r
    }

    async fn dispatch(op: ResourceOp, req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup("model").expect("model registered");
        let handler = def.operations.get(&op).expect("op registered").clone();
        let ctx = ToolContext::for_op("model", op, req.resource_id.clone());
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[test]
    fn registers_model_resource() {
        let r = reg();
        assert!(r.lookup("model").is_some());
    }

    #[test]
    fn supports_list_create_delete_execute() {
        let r = reg();
        let def = r.lookup("model").unwrap();
        assert_eq!(
            def.supported_ops(),
            vec![
                ResourceOp::List,
                ResourceOp::Create,
                ResourceOp::Delete,
                ResourceOp::Execute,
            ]
        );
    }

    #[test]
    fn destructive_classification_matches_named_tools() {
        let r = reg();
        let def = r.lookup("model").unwrap();
        // preload (create), evict (delete), auto_evict (execute) mutate VRAM.
        assert!(def.is_destructive(ResourceOp::Create));
        assert!(def.is_destructive(ResourceOp::Delete));
        assert!(def.is_destructive(ResourceOp::Execute));
        // list is read-only.
        assert!(!def.is_destructive(ResourceOp::List));
    }

    #[test]
    fn model_is_not_searchable() {
        let r = reg();
        let types = r.searchable_types(&ToolsetFilter::all());
        assert!(types.is_empty(), "model has no search op");
    }

    #[test]
    fn describe_lists_all_ops_with_flags() {
        let v = describe_model();
        let ops = v["operations"].as_object().unwrap();
        for op in ["list", "create", "delete", "execute"] {
            assert!(ops.contains_key(op), "describe missing {op}");
        }
        assert_eq!(ops["create"]["destructive"], true);
        assert_eq!(ops["delete"]["destructive"], true);
        assert_eq!(ops["execute"]["destructive"], true);
        assert_eq!(ops["list"]["destructive"], false);
        let actions = ops["execute"]["actions"].as_array().unwrap();
        assert_eq!(actions[0]["name"], "auto_evict_lru");
    }

    // ── argument-validation round-trips (no wylde-ollama needed) ──────

    #[tokio::test]
    async fn create_without_model_errors_cleanly() {
        let out = dispatch(ResourceOp::Create, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("model"));
    }

    #[tokio::test]
    async fn delete_without_id_errors_cleanly() {
        let out = dispatch(ResourceOp::Delete, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("resource_id"));
    }

    #[tokio::test]
    async fn execute_missing_action_errors_cleanly() {
        let out = dispatch(ResourceOp::Execute, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["auto_evict_lru"]));
    }

    #[tokio::test]
    async fn execute_unknown_action_errors_cleanly() {
        let out = dispatch(
            ResourceOp::Execute,
            ResourceRequest {
                action: Some("bogus".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["auto_evict_lru"]));
    }

    #[tokio::test]
    async fn delete_resource_id_supplies_model_tag() {
        // resource_id carries the tag; with no wylde-ollama running the
        // adapter forwards to run_evict, which surfaces an "unreachable"
        // envelope — NOT a "missing model" error. Proves the tag threaded.
        let out = dispatch(
            ResourceOp::Delete,
            ResourceRequest {
                resource_id: Some("qwen2.5:7b".into()),
                ..Default::default()
            },
        )
        .await;
        if out["status"] == "error" {
            assert!(!out["error"]
                .as_str()
                .unwrap()
                .contains("requires 'resource_id'"));
        }
    }
}
