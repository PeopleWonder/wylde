//! `memory` resource — the first real verb migration (consolidation
//! Slice 2, `docs/plans/tool-registry-consolidation.md` §6).
//!
//! Lights up the long-term memory cluster under the generic verbs:
//!
//! | Verb call | Delegates to |
//! |---|---|
//! | `wylde_search("memory", query, {limit, decay_days, query_vector, scope})` | [`tools_memory::run_search`] |
//! | `wylde_get("memory", resource_id)` | [`long_term::get`] |
//! | `wylde_create("memory", {scope, body, source, importance, tags, vector})` | [`tools_memory::run_save`] |
//! | `wylde_update("memory", resource_id, {body, importance, source, vector})` | [`tools_memory::run_update`] |
//! | `wylde_delete("memory", resource_id)` | [`tools_memory::run_delete`] |
//!
//! ## Adapter pattern — no logic duplication
//!
//! Each [`OpHandler`] reshapes its [`ResourceRequest`] into the `args`
//! object the existing `memory.*` tool handler already accepts, then
//! calls straight through. The handlers were made `pub(crate)` for this;
//! the named `memory_search` / `memory_long_term_save` / … tools stay
//! registered and unchanged (plan §6 — both surfaces run in parallel
//! until the Slice-6 cutover behind `WYLDE_HARNESS_VERB_TOOLS`).
//!
//! ## Scope dispatch on `create`
//!
//! `wylde_create` dispatches on `body.scope`: `"long_term"` (or absent)
//! routes to the long-term save handler. `"workspace"` is **not yet
//! ported to Rust** — the durable workspace-memory *save* path has no
//! Rust handler (only the workspace *registry* exists, see
//! `crate::memory::workspaces`), so the verb returns an explicit
//! `not_supported` envelope rather than silently writing to the wrong
//! tier. When that handler lands, this dispatch grows a second arm with
//! no change to the verb surface.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::memory::long_term;
use crate::tooling::resource::definition::{
    op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope, ToolContext,
};
use crate::tooling::resource::ResourceRegistry;
use crate::tooling::tools::memory as tools_memory;

/// Register the `memory` resource into the built-in registry.
pub fn register_memory_resource(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();

    operations.insert(ResourceOp::Search, op_handler(op_search));
    operations.insert(ResourceOp::Get, op_handler(op_get));
    operations.insert(ResourceOp::Create, op_handler(op_create));
    operations.insert(ResourceOp::Update, op_handler(op_update));
    operations.insert(ResourceOp::Delete, op_handler(op_delete));

    reg.register_builtin(ResourceDefinition {
        resource_type: "memory",
        display_name: "Long-term memory",
        description: "Global, cross-workspace, user-visible memories that persist \
                      across conversations. Vector + recency-decay search; CRUD by id.",
        scope: Scope::Global,
        identifier_fields: &["id"],
        filter_fields: &["limit", "decay_days", "query_vector", "scope"],
        operations,
        // get/search read only; create/update/delete mutate.
        destructive_ops: &[ResourceOp::Create, ResourceOp::Update, ResourceOp::Delete],
        describe: describe_memory,
    });
}

// ── OpHandlers — thin adapters over the named-tool handlers ──────────

/// `wylde_search("memory", …)` → `memory_search`. The verb's top-level
/// `query` / `limit` overlay the `filter` object, so callers may pass
/// `decay_days` / `query_vector` / `scope` inside `filter`.
fn op_search(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(q) = req.query {
        args.insert("query".into(), json!(q));
    }
    if let Some(l) = req.limit {
        args.insert("limit".into(), json!(l));
    }
    async move { tools_memory::run_search(Value::Object(args)).await }
}

/// `wylde_get("memory", id)` — direct lookup against the long-term
/// store. There is no `memory_get` named tool, so this calls the memory
/// layer ([`long_term::get`]) rather than a tool handler; still a thin
/// Rust adapter, not reimplemented logic.
async fn op_get(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> Result<Value, IpcError> {
    let Some(id) = req.resource_id else {
        return Ok(json!({
            "status": "error",
            "error": "wylde_get(\"memory\", …) requires 'resource_id'",
        }));
    };
    match long_term::get(&id) {
        Some(rec) => Ok(json!({
            "status": "success",
            "memory": serde_json::to_value(&rec).unwrap_or(Value::Null),
        })),
        None => Ok(json!({
            "status": "error",
            "error": format!("memory not found: {id}"),
            "code": "not_found",
        })),
    }
}

/// `wylde_create("memory", {scope, …})` → `memory_long_term_save`,
/// dispatching on `body.scope`.
fn op_create(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let body = as_object(req.body);
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("long_term")
        .trim()
        .to_owned();
    async move {
        match scope.as_str() {
            "" | "long_term" => tools_memory::run_save(Value::Object(body)).await,
            "workspace" => Ok(json!({
                "status": "not_supported",
                "error": "workspace-scoped memory save is not ported to Rust yet; \
                          only scope=\"long_term\" is available via wylde_create. \
                          (The workspace memory registry exists but the durable \
                          save handler does not — see crate::memory::workspaces.)",
                "scope": "workspace",
            })),
            other => Ok(json!({
                "status": "error",
                "error": format!(
                    "unknown memory scope {other:?}; expected \"long_term\" \
                     (workspace not yet supported)"
                ),
            })),
        }
    }
}

/// `wylde_update("memory", id, body)` → `memory_update`. The verb's
/// `resource_id` becomes the handler's `memory_id`.
fn op_update(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.body);
    if let Some(id) = req.resource_id {
        args.insert("memory_id".into(), json!(id));
    }
    async move { tools_memory::run_update(Value::Object(args)).await }
}

/// `wylde_delete("memory", id)` → `memory_delete`.
fn op_delete(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let args = match req.resource_id {
        Some(id) => json!({ "memory_id": id }),
        None => json!({}),
    };
    async move { tools_memory::run_delete(args).await }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Coerce a `Value` into an owned object map — `Null` / non-objects
/// become an empty map so handlers always see a well-formed `args`.
fn as_object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

// ── describe() ───────────────────────────────────────────────────────

/// Full self-description for `wylde_describe(resource_type="memory")`.
/// Each operation carries a JSON Schema for its arguments, mirroring the
/// existing `memory.*` tool argument shapes (requirement #3).
fn describe_memory() -> Value {
    json!({
        "resource_type": "memory",
        "display_name": "Long-term memory",
        "description": "Global, cross-workspace, user-visible memories that persist \
                        across conversations. Vector + recency-decay search; CRUD by id.",
        "scope": "global",
        "identifier_fields": ["id"],
        "filter_fields": ["limit", "decay_days", "query_vector", "scope"],
        "operations": {
            "search": {
                "verb": "wylde_search",
                "destructive": false,
                "description": "Vector + recency-decay search over long-term memory. \
                                Pass `query` (embedded via wylde-ollama) or a precomputed \
                                `query_vector` inside `filter`.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Text query (embedded via wylde-ollama)"},
                        "filter": {
                            "type": "object",
                            "properties": {
                                "query_vector": {"type": "array", "items": {"type": "number"}, "description": "Precomputed embedding (alternative to query)"},
                                "limit": {"type": "number", "description": "Max hits (default 5)"},
                                "decay_days": {"type": "number", "description": "Recency decay constant (default 30)"},
                                "scope": {"type": "string", "enum": ["long_term"], "description": "Memory tier (advisory)"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max hits (default 5); overlays filter.limit"}
                    }
                }
            },
            "get": {
                "verb": "wylde_get",
                "destructive": false,
                "description": "Fetch one memory record by id.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Memory id"}
                    },
                    "required": ["resource_id"]
                }
            },
            "create": {
                "verb": "wylde_create",
                "destructive": true,
                "description": "Save a new memory. Dispatch on body.scope: \"long_term\" \
                                (default) saves a global record. \"workspace\" is not yet \
                                supported in Rust.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "scope": {"type": "string", "enum": ["long_term"], "description": "Memory tier (default long_term)"},
                                "body": {"type": "string", "description": "Memory text"},
                                "source": {"type": "string", "description": "Origin tag"},
                                "importance": {"type": "number", "description": "Importance 0..10"},
                                "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tag list"},
                                "vector": {"type": "array", "items": {"type": "number"}, "description": "Precomputed embedding"}
                            },
                            "required": ["body"]
                        }
                    }
                }
            },
            "update": {
                "verb": "wylde_update",
                "destructive": true,
                "description": "Revise a memory. Writes a new version and supersedes the old.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Memory id to revise"},
                        "body": {
                            "type": "object",
                            "properties": {
                                "body": {"type": "string", "description": "New body (optional)"},
                                "importance": {"type": "number", "description": "New importance"},
                                "source": {"type": "string", "description": "New source tag"},
                                "vector": {"type": "array", "items": {"type": "number"}, "description": "Precomputed embedding"}
                            }
                        }
                    },
                    "required": ["resource_id"]
                }
            },
            "delete": {
                "verb": "wylde_delete",
                "destructive": true,
                "description": "Permanently remove a memory and anything superseded by it.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Memory id"}
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
    use crate::memory::long_term::test_support::TestEnv;
    use crate::tooling::resource::ToolsetFilter;

    fn cfg() -> &'static crate::config::Config {
        Box::leak(Box::new(crate::config::Config::default_for_tests()))
    }

    fn set_embed_dim_3() {
        std::env::set_var("WYLDE_EMBED_DIM", "3");
    }

    /// A registry with only the memory resource — exercises the exact
    /// wiring `register_resources` produces, in isolation.
    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_memory_resource(&mut r);
        r
    }

    /// Run an op through the registered `OpHandler` (the real verb-layer
    /// dispatch path: lookup the def, fetch the handler, call it).
    async fn dispatch(op: ResourceOp, req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup("memory").expect("memory registered");
        let handler = def.operations.get(&op).expect("op registered").clone();
        let ctx = ToolContext::for_op("memory", op, req.resource_id.clone());
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[test]
    fn registers_memory_with_five_ops() {
        let r = reg();
        let def = r.lookup("memory").unwrap();
        assert_eq!(def.resource_type, "memory");
        assert_eq!(def.scope, Scope::Global);
        assert_eq!(
            def.supported_ops(),
            vec![
                ResourceOp::Get,
                ResourceOp::Create,
                ResourceOp::Update,
                ResourceOp::Delete,
                ResourceOp::Search,
            ]
        );
    }

    #[test]
    fn create_update_delete_are_destructive_get_search_are_not() {
        let r = reg();
        let def = r.lookup("memory").unwrap();
        assert!(def.is_destructive(ResourceOp::Create));
        assert!(def.is_destructive(ResourceOp::Update));
        assert!(def.is_destructive(ResourceOp::Delete));
        assert!(!def.is_destructive(ResourceOp::Get));
        assert!(!def.is_destructive(ResourceOp::Search));
    }

    #[test]
    fn memory_is_searchable_and_visible_at_global_scope() {
        let r = reg();
        let filter = ToolsetFilter::all();
        assert_eq!(r.searchable_types(&filter), vec!["memory".to_string()]);
        assert!(r.lookup_visible("memory", &filter).is_some());
    }

    #[test]
    fn describe_lists_all_five_operations() {
        let v = describe_memory();
        assert_eq!(v["resource_type"], "memory");
        let ops = v["operations"].as_object().unwrap();
        for op in ["search", "get", "create", "update", "delete"] {
            assert!(ops.contains_key(op), "describe missing op {op}");
        }
        // Destructive flags in describe match destructive_ops.
        assert_eq!(ops["create"]["destructive"], true);
        assert_eq!(ops["search"]["destructive"], false);
    }

    #[tokio::test]
    async fn create_then_get_round_trips_via_verb_path() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let created = dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"body": "verb-created memory", "importance": 7, "vector": [1.0, 0.0, 0.0]}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(created["status"], "success");
        let id = created["id"].as_str().unwrap().to_owned();

        let got = dispatch(
            ResourceOp::Get,
            ResourceRequest {
                resource_id: Some(id.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got["status"], "success");
        assert_eq!(got["memory"]["body"], "verb-created memory");
        assert_eq!(got["memory"]["id"], id);
    }

    #[tokio::test]
    async fn create_matches_named_tool_output_shape() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // Verb path.
        let verb = dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"body": "parity body", "importance": 6}),
                ..Default::default()
            },
        )
        .await;
        // Named-tool path (same handler), separate record.
        let named = tools_memory::run_save(json!({"body": "parity body", "importance": 6}))
            .await
            .unwrap();
        // Same envelope keys + types — the verb adds nothing, strips nothing.
        for k in ["status", "body", "importance", "created_at"] {
            assert_eq!(verb[k].is_null(), named[k].is_null(), "key {k}");
        }
        assert_eq!(verb["status"], named["status"]);
        assert_eq!(verb["body"], named["body"]);
        assert_eq!(verb["importance"], named["importance"]);
    }

    #[tokio::test]
    async fn create_scope_long_term_is_default() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let out = dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"scope": "long_term", "body": "explicit lt"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
    }

    #[tokio::test]
    async fn create_scope_workspace_is_not_supported() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let out = dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"scope": "workspace", "body": "ws"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "not_supported");
        assert_eq!(out["scope"], "workspace");
    }

    #[tokio::test]
    async fn update_supersedes_via_verb_path() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let created = dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"body": "v1", "importance": 5}),
                ..Default::default()
            },
        )
        .await;
        let orig_id = created["id"].as_str().unwrap().to_owned();

        let updated = dispatch(
            ResourceOp::Update,
            ResourceRequest {
                resource_id: Some(orig_id.clone()),
                body: json!({"body": "v2", "importance": 9}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(updated["status"], "success");
        assert_eq!(updated["importance"], 9);
        let new_id = updated["id"].as_str().unwrap();
        assert_ne!(new_id, orig_id);
        assert_eq!(long_term::get(&orig_id).unwrap().superseded_by, new_id);
    }

    #[tokio::test]
    async fn update_unknown_id_is_not_found() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let out = dispatch(
            ResourceOp::Update,
            ResourceRequest {
                resource_id: Some("ghost".into()),
                body: json!({"body": "x"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "not_found");
    }

    #[tokio::test]
    async fn delete_removes_via_verb_path() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let created = dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"body": "doomed", "importance": 5}),
                ..Default::default()
            },
        )
        .await;
        let id = created["id"].as_str().unwrap().to_owned();
        let out = dispatch(
            ResourceOp::Delete,
            ResourceRequest {
                resource_id: Some(id.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        assert!(long_term::get(&id).is_none());
    }

    #[tokio::test]
    async fn delete_without_id_errors_cleanly() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let out = dispatch(ResourceOp::Delete, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
    }

    #[tokio::test]
    async fn get_unknown_id_is_not_found() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let out = dispatch(
            ResourceOp::Get,
            ResourceRequest {
                resource_id: Some("nope".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "not_found");
    }

    #[tokio::test]
    async fn search_via_verb_path_matches_query_vector() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // Seed a record with a known vector.
        dispatch(
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"body": "near", "importance": 6, "vector": [1.0, 0.0, 0.0]}),
                ..Default::default()
            },
        )
        .await;
        // Search via the precomputed-vector path (no embedder needed) —
        // query_vector travels inside `filter`.
        let out = dispatch(
            ResourceOp::Search,
            ResourceRequest {
                filter: json!({"query_vector": [1.0, 0.0, 0.0]}),
                limit: Some(5),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        let results = out["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["body"], "near");
    }
}
