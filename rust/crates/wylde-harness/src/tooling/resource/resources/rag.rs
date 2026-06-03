//! RAG + knowledge-graph resources (tool-registry consolidation Slice 3,
//! `docs/plans/tool-registry-consolidation.md` §5).
//!
//! Collapses the eight `rag.*` named tools + `meta.graph_query` into the
//! generic verb surface. Every [`OpHandler`] is a **thin adapter** that
//! reshapes its [`ResourceRequest`] into the `args` object the existing
//! `crate::memory::rag::actions` / `meta` handler already accepts, then
//! calls straight through — no retrieval / embedding / prune logic is
//! duplicated, and the old named tools stay registered in parallel until
//! the Slice-6 cutover behind `WYLDE_HARNESS_VERB_TOOLS`.
//!
//! ## Resource map
//!
//! | Resource | Op | Delegates to | Notes |
//! |---|---|---|---|
//! | `rag_chunk` | search | [`actions::run_rag_search`] | embed-wired (S2a) — embeds `q` server-side |
//! | `rag_chunk` | create | [`actions::run_rag_add_episodic`] | writes an **episodic**-tier chunk |
//! | `rag_chunk` | delete | [`actions::run_rag_prune`] | filter form (`before_ts`/`memory_type`/`score_lt`), dry-run unless `confirm` |
//! | `rag` | execute | [`actions::run_rag_index`] / [`actions::run_rag_reindex`] | fire-and-forget N8N ingest triggers (`action`=`index`/`reindex`) |
//! | `rag_feedback` | create | [`actions::run_rag_feedback`] | ±1/0 rating for a prior search `query_id` |
//! | `rag_miss` | list | [`actions::run_rag_misses`] | recent retrieval misses |
//! | `rag_chunk_usage` | list | [`actions::run_rag_chunk_usage`] | per-chunk retrieval counts |
//! | `rag_graph_stats` | get | [`actions::run_rag_graph_stats`] | Memgraph node/edge counts |
//! | `graph` | search | [`crate::memory::memgraph::actions::run_graph_query`] | entity / vector hybrid traversal |
//!
//! ## Why `rag` (execute) is split from `rag_chunk` (CRUD)
//!
//! `rag.index` / `rag.reindex` are fire-and-forget N8N webhook triggers,
//! not mutations of a resource you can name — they rebuild the index as a
//! whole. They map to `wylde_execute("rag", "index"|"reindex", …)` (plan
//! §7: "actions on the index", kept off the imperative tail since they are
//! neither device-lifecycle nor arbitrary code). The stored units those
//! pipelines produce are `rag_chunk`s, which is where the CRUD verbs live.
//!
//! ## Search routes through the embed-wired `rag.search`, not `rag.ask`
//!
//! `rag.ask` (the model-callable named tool) deliberately refuses to embed
//! and returns `insufficient_context` without a precomputed `query_vector`.
//! `rag.search` (the Wylde_Study S2a pipe verb) embeds `q` server-side via
//! wylde-ollama, then runs the identical first-party search. The verb
//! surface wants the embed-wired path, so `wylde_search("rag_chunk", q)`
//! delegates to [`actions::run_rag_search`]; a precomputed `query_vector`
//! inside `filter` still short-circuits the embed round-trip.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::memory::rag::actions;
use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;

/// Register every RAG + graph resource into the built-in registry.
pub fn register_rag_resources(reg: &mut ResourceRegistry) {
    register_rag_chunk(reg);
    register_rag_index(reg);
    register_rag_feedback(reg);
    register_rag_miss(reg);
    register_rag_chunk_usage(reg);
    register_rag_graph_stats(reg);
    register_graph(reg);
}

// ── rag_chunk — the tiered RAG store unit (search / create / delete) ──

fn register_rag_chunk(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Search, op_handler(op_chunk_search));
    operations.insert(ResourceOp::Create, op_handler(op_chunk_create));
    operations.insert(ResourceOp::Delete, op_handler(op_chunk_delete));

    reg.register_builtin(ResourceDefinition {
        resource_type: "rag_chunk",
        display_name: "RAG memory chunk",
        description: "Units of the tiered RAG vector store. Search embeds the query \
                      server-side; create adds an episodic-tier chunk; delete prunes by \
                      filter (dry-run unless confirm=true).",
        scope: Scope::Global,
        identifier_fields: &["id", "memory_id"],
        filter_fields: &[
            "query_vector", "limit", "tier", "workspace", "workspace_id",
            "before_ts", "memory_type", "score_lt", "confirm", "max_delete",
        ],
        operations,
        // create writes a chunk; delete prunes. search reads only.
        destructive_ops: &[ResourceOp::Delete],
        describe: describe_value(describe_rag_chunk),
    });
}

/// `wylde_search("rag_chunk", q, {query_vector?, tier?, workspace?, limit?})`
/// → [`actions::run_rag_search`] (embed-wired). `query` becomes `q`.
fn op_chunk_search(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(q) = req.query {
        args.insert("q".into(), json!(q));
    }
    if let Some(l) = req.limit {
        args.insert("limit".into(), json!(l));
    }
    async move { actions::run_rag_search(Value::Object(args)).await }
}

/// `wylde_create("rag_chunk", {content, source_path?, session_id?, score?, vector?})`
/// → [`actions::run_rag_add_episodic`]. Writes an episodic-tier record (the
/// only insert handler that exists in Rust today).
fn op_chunk_create(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let args = as_object(req.body);
    async move { actions::run_rag_add_episodic(Value::Object(args)).await }
}

/// `wylde_delete("rag_chunk", filter={before_ts?, memory_type?, score_lt?, confirm?, max_delete?})`
/// → [`actions::run_rag_prune`]. The handler requires at least one filter
/// and dry-runs unless `confirm=true`.
fn op_chunk_delete(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    // prune is filter-driven, never id-driven; fold the verb's `filter`
    // (and tolerate the model passing the predicate under `body`).
    let mut args = as_object(req.filter);
    if args.is_empty() {
        args = as_object(req.body);
    }
    async move { actions::run_rag_prune(Value::Object(args)).await }
}

// ── rag — index / reindex pipeline triggers (execute) ────────────────

fn register_rag_index(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Execute, op_handler(op_rag_execute));

    reg.register_builtin(ResourceDefinition {
        resource_type: "rag",
        display_name: "RAG index",
        description: "The RAG index as a whole. execute(action='index') triggers an \
                      incremental ingest run; execute(action='reindex') wipes and rebuilds. \
                      Both are fire-and-forget N8N webhook triggers.",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &[],
        operations,
        destructive_ops: &[ResourceOp::Execute],
        describe: describe_value(describe_rag_index),
    });
}

/// `wylde_execute("rag", "index"|"reindex", params)` → the matching
/// ingest trigger. `params` carries `target_path`/`workspace_id`/`paths`/
/// `force` straight through to the handler.
fn op_rag_execute(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let action = req.action.clone().unwrap_or_default();
    let args = as_object(req.params);
    async move {
        match action.as_str() {
            "index" => actions::run_rag_index(Value::Object(args)).await,
            "reindex" => actions::run_rag_reindex(Value::Object(args)).await,
            "" => Ok(json!({
                "status": "error",
                "error": "wylde_execute(\"rag\", …) requires an 'action' of \"index\" or \"reindex\"",
                "known_actions": ["index", "reindex"],
            })),
            other => Ok(json!({
                "status": "error",
                "error": format!("unknown rag action {other:?}; expected \"index\" or \"reindex\""),
                "known_actions": ["index", "reindex"],
            })),
        }
    }
}

// ── rag_feedback — create a rating for a prior search ────────────────

fn register_rag_feedback(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Create, op_handler(op_feedback_create));

    reg.register_builtin(ResourceDefinition {
        resource_type: "rag_feedback",
        display_name: "RAG retrieval feedback",
        description: "Attach a rating (+1 helpful, 0 neutral, -1 bad) to a prior search's \
                      query_id. Persisted via the miss_log layer.",
        scope: Scope::Global,
        identifier_fields: &["query_id"],
        filter_fields: &[],
        operations,
        // The named `rag.feedback` tool is non-destructive (it writes a
        // rating row, not a data mutation): keep the *fine* gate off. The
        // coarse `wylde_create` gate still applies — see the module note.
        destructive_ops: &[],
        describe: describe_value(describe_rag_feedback),
    });
}

/// `wylde_create("rag_feedback", {query_id, score, comment?})` →
/// [`actions::run_rag_feedback`].
fn op_feedback_create(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.body);
    // Allow the model to pass query_id via the verb's resource_id too.
    if let Some(id) = req.resource_id {
        args.entry("query_id").or_insert_with(|| json!(id));
    }
    async move { actions::run_rag_feedback(Value::Object(args)).await }
}

// ── rag_miss — list recent retrieval misses ──────────────────────────

fn register_rag_miss(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(op_miss_list));

    reg.register_builtin(ResourceDefinition {
        resource_type: "rag_miss",
        display_name: "RAG retrieval miss",
        description: "Recent queries where retrieval missed (confidence gate fired or no \
                      valid citations). Read from the miss_log layer.",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &["only_gated", "include_trace", "since", "limit"],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_rag_miss),
    });
}

/// `wylde_list("rag_miss", {only_gated?, since?, limit?})` →
/// [`actions::run_rag_misses`].
fn op_miss_list(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(l) = req.limit {
        args.insert("limit".into(), json!(l));
    }
    async move { actions::run_rag_misses(Value::Object(args)).await }
}

// ── rag_chunk_usage — list per-chunk retrieval counts ────────────────

fn register_rag_chunk_usage(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(op_chunk_usage_list));

    reg.register_builtin(ResourceDefinition {
        resource_type: "rag_chunk_usage",
        display_name: "RAG chunk usage",
        description: "Per-chunk retrieval counts from the miss_log layer. dead_only=true \
                      returns only chunks never cited.",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &["dead_only", "limit"],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_rag_chunk_usage),
    });
}

/// `wylde_list("rag_chunk_usage", {dead_only?, limit?})` →
/// [`actions::run_rag_chunk_usage`].
fn op_chunk_usage_list(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(l) = req.limit {
        args.insert("limit".into(), json!(l));
    }
    async move { actions::run_rag_chunk_usage(Value::Object(args)).await }
}

// ── rag_graph_stats — get Memgraph node/edge counts ──────────────────

fn register_rag_graph_stats(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Get, op_handler(op_graph_stats_get));

    reg.register_builtin(ResourceDefinition {
        resource_type: "rag_graph_stats",
        display_name: "RAG graph stats",
        description: "Node/edge counts (entities, chunks, mentions) in the Memgraph service. \
                      Returns reachable=false with zeros when the backend is down — never raises.",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &[],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_rag_graph_stats),
    });
}

/// `wylde_get("rag_graph_stats")` → [`actions::run_rag_graph_stats`].
/// A singleton resource — no id needed.
async fn op_graph_stats_get(
    _req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> Result<Value, IpcError> {
    actions::run_rag_graph_stats(Value::Null).await
}

// ── graph — entity / vector hybrid traversal (search) ────────────────

fn register_graph(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Search, op_handler(op_graph_search));

    reg.register_builtin(ResourceDefinition {
        resource_type: "graph",
        display_name: "Knowledge graph",
        description: "Traverse the Memgraph knowledge graph from entity seeds, a query, or a \
                      precomputed query_vector (hybrid vector+graph). Returns chunks ranked by \
                      graph proximity.",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &["entities", "query_vector", "max_hops", "vector_k", "tier", "workspace_id", "limit"],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_graph),
    });
}

/// `wylde_search("graph", q, {entities?, query_vector?, max_hops?, …})` →
/// [`crate::memory::memgraph::actions::run_graph_query`]. Routed through
/// the same strangler dispatch the `meta.graph_query` named tool uses
/// (`current_traversal_impl()` selects Bolt vs pipe).
fn op_graph_search(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(q) = req.query {
        args.insert("q".into(), json!(q));
    }
    if let Some(l) = req.limit {
        args.insert("limit".into(), json!(l));
    }
    async move {
        let client = crate::memory::memgraph::current_traversal_impl();
        crate::memory::memgraph::actions::run_graph_query(Value::Object(args), &client).await
    }
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

fn describe_rag_chunk() -> Value {
    json!({
        "resource_type": "rag_chunk",
        "display_name": "RAG memory chunk",
        "description": "Units of the tiered RAG vector store.",
        "scope": "global",
        "identifier_fields": ["id", "memory_id"],
        "operations": {
            "search": {
                "verb": "wylde_search",
                "destructive": false,
                "description": "Embed-wired search (rag.search). Pass `query` (embedded \
                                server-side) or a precomputed `query_vector` in `filter`.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Natural-language query (embedded server-side)"},
                        "filter": {
                            "type": "object",
                            "properties": {
                                "query_vector": {"type": "array", "items": {"type": "number"}, "description": "Precomputed embedding (skips embed round-trip)"},
                                "tier": {"type": "string", "description": "Restrict to one tier (core/episodic/semantic/procedural)"},
                                "workspace": {"type": "string", "description": "Workspace id (miss-log attribution)"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max hits 1..50 (default 8)"}
                    }
                }
            },
            "create": {
                "verb": "wylde_create",
                "destructive": false,
                "description": "Add an episodic-tier chunk (rag.add_episodic).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "Chunk text (alias: text)"},
                                "source_path": {"type": "string", "description": "Origin path/url"},
                                "session_id": {"type": "string", "description": "Optional session tag"},
                                "score": {"type": "number", "description": "Initial relevance score"},
                                "vector": {"type": "array", "items": {"type": "number"}, "description": "Precomputed embedding"}
                            },
                            "required": ["content"]
                        }
                    }
                }
            },
            "delete": {
                "verb": "wylde_delete",
                "destructive": true,
                "description": "Prune chunks matching a filter (rag.prune). At least one of \
                                before_ts/memory_type/score_lt required; dry-runs unless confirm=true.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "before_ts": {"type": "number", "description": "Delete chunks created before this unix ts"},
                                "memory_type": {"type": "string", "description": "Delete only this tier"},
                                "score_lt": {"type": "number", "description": "Delete chunks scoring below this"},
                                "confirm": {"type": "boolean", "description": "Must be true to actually delete (default false = dry-run)"},
                                "max_delete": {"type": "number", "description": "Safety cap 1..10000 (default 500)"}
                            }
                        }
                    }
                }
            }
        }
    })
}

fn describe_rag_index() -> Value {
    json!({
        "resource_type": "rag",
        "display_name": "RAG index",
        "description": "The RAG index as a whole — execute ingest triggers.",
        "scope": "global",
        "operations": {
            "execute": {
                "verb": "wylde_execute",
                "destructive": true,
                "description": "Trigger an ingest run. action='index' (incremental) or \
                                action='reindex' (wipe + rebuild). Fire-and-forget via N8N.",
                "actions": [
                    {"name": "index", "description": "Incremental indexing over source paths"},
                    {"name": "reindex", "description": "Wipe and rebuild the whole index"}
                ],
                "schema": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["index", "reindex"]},
                        "params": {
                            "type": "object",
                            "properties": {
                                "target_path": {"type": "string", "description": "Workspace root the ingest walks"},
                                "workspace_id": {"type": "string", "description": "Logical workspace bucket (default 'default')"},
                                "paths": {"type": "array", "items": {"type": "string"}, "description": "index-only: paths relative to target_path"},
                                "force": {"type": "boolean", "description": "index-only: re-index even unchanged files"}
                            }
                        }
                    },
                    "required": ["action"]
                }
            }
        }
    })
}

fn describe_rag_feedback() -> Value {
    json!({
        "resource_type": "rag_feedback",
        "display_name": "RAG retrieval feedback",
        "description": "Rate a prior search by query_id.",
        "scope": "global",
        "identifier_fields": ["query_id"],
        "operations": {
            "create": {
                "verb": "wylde_create",
                "destructive": false,
                "description": "Record feedback (rag.feedback).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "query_id": {"type": "string", "description": "query_id from a prior search"},
                                "score": {"type": "number", "description": "-1, 0, or 1"},
                                "comment": {"type": "string", "description": "Optional rationale"}
                            },
                            "required": ["query_id", "score"]
                        }
                    }
                }
            }
        }
    })
}

fn describe_rag_miss() -> Value {
    json!({
        "resource_type": "rag_miss",
        "display_name": "RAG retrieval miss",
        "description": "Recent retrieval misses from the miss_log layer.",
        "scope": "global",
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "List recent misses (rag.misses).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "only_gated": {"type": "boolean", "description": "Restrict to gated rows (default true)"},
                                "include_trace": {"type": "boolean", "description": "Include retrieval-trace JSON"},
                                "since": {"type": "number", "description": "Only rows with ts >= this"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max rows 1..1000 (default 100)"}
                    }
                }
            }
        }
    })
}

fn describe_rag_chunk_usage() -> Value {
    json!({
        "resource_type": "rag_chunk_usage",
        "display_name": "RAG chunk usage",
        "description": "Per-chunk retrieval counts.",
        "scope": "global",
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "List per-chunk retrieval counts (rag.chunk_usage).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "dead_only": {"type": "boolean", "description": "Only chunks never cited"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max rows 1..10000 (default 100)"}
                    }
                }
            }
        }
    })
}

fn describe_rag_graph_stats() -> Value {
    json!({
        "resource_type": "rag_graph_stats",
        "display_name": "RAG graph stats",
        "description": "Memgraph node/edge counts (singleton).",
        "scope": "global",
        "operations": {
            "get": {
                "verb": "wylde_get",
                "destructive": false,
                "description": "Fetch node/edge counts (rag.graph_stats). No id required.",
                "schema": {"type": "object", "properties": {}}
            }
        }
    })
}

fn describe_graph() -> Value {
    json!({
        "resource_type": "graph",
        "display_name": "Knowledge graph",
        "description": "Entity / vector hybrid traversal of the Memgraph graph.",
        "scope": "global",
        "operations": {
            "search": {
                "verb": "wylde_search",
                "destructive": false,
                "description": "Traverse from entity seeds, a query, or a query_vector (graph_query).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Query — identifiers extracted as entity seeds"},
                        "filter": {
                            "type": "object",
                            "properties": {
                                "entities": {"type": "array", "items": {"type": "string"}, "description": "Explicit entity seeds (skips extraction)"},
                                "query_vector": {"type": "array", "items": {"type": "number"}, "description": "Precomputed embedding — enables hybrid path"},
                                "max_hops": {"type": "number", "description": "Expansion depth 1..4 (default 1)"},
                                "vector_k": {"type": "number", "description": "Vector seeds 1..20 (default 5)"},
                                "tier": {"type": "string", "description": "Restrict vector search to one tier"},
                                "workspace_id": {"type": "string", "description": "Filter chunks to this workspace"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max chunks 1..50 (default 10)"}
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::resource::ToolsetFilter;

    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_rag_resources(&mut r);
        r
    }

    #[test]
    fn registers_seven_rag_resources() {
        let r = reg();
        for rt in [
            "rag_chunk", "rag", "rag_feedback", "rag_miss",
            "rag_chunk_usage", "rag_graph_stats", "graph",
        ] {
            assert!(r.lookup(rt).is_some(), "missing resource {rt}");
        }
        assert_eq!(r.builtin_len(), 7);
    }

    #[test]
    fn rag_chunk_ops_and_destructive_classification() {
        let r = reg();
        let def = r.lookup("rag_chunk").unwrap();
        assert_eq!(
            def.supported_ops(),
            vec![ResourceOp::Create, ResourceOp::Delete, ResourceOp::Search]
        );
        // Only delete (prune) is destructive; search + create are not.
        assert!(def.is_destructive(ResourceOp::Delete));
        assert!(!def.is_destructive(ResourceOp::Create));
        assert!(!def.is_destructive(ResourceOp::Search));
    }

    #[test]
    fn rag_index_execute_is_destructive() {
        let r = reg();
        let def = r.lookup("rag").unwrap();
        assert_eq!(def.supported_ops(), vec![ResourceOp::Execute]);
        assert!(def.is_destructive(ResourceOp::Execute));
    }

    #[test]
    fn searchable_types_are_rag_chunk_and_graph() {
        let r = reg();
        let mut types = r.searchable_types(&ToolsetFilter::all());
        types.sort();
        assert_eq!(types, vec!["graph".to_string(), "rag_chunk".to_string()]);
    }

    #[test]
    fn describe_rag_chunk_lists_three_ops_with_correct_flags() {
        let v = describe_rag_chunk();
        let ops = v["operations"].as_object().unwrap();
        for op in ["search", "create", "delete"] {
            assert!(ops.contains_key(op), "describe missing {op}");
        }
        assert_eq!(ops["delete"]["destructive"], true);
        assert_eq!(ops["search"]["destructive"], false);
        assert_eq!(ops["create"]["destructive"], false);
    }

    #[test]
    fn describe_rag_index_enumerates_actions() {
        let v = describe_rag_index();
        let actions = v["operations"]["execute"]["actions"].as_array().unwrap();
        let names: Vec<&str> = actions.iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["index", "reindex"]);
    }

    // ── round-trips through the real OpHandler dispatch path ──────────

    fn cfg() -> &'static crate::config::Config {
        Box::leak(Box::new(crate::config::Config::default_for_tests()))
    }

    async fn dispatch(rt: &str, op: ResourceOp, req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup(rt).expect("resource registered");
        let handler = def.operations.get(&op).expect("op registered").clone();
        let ctx = ToolContext::for_op(rt, op, req.resource_id.clone());
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[tokio::test]
    async fn rag_chunk_search_missing_query_errors_cleanly() {
        // No `q` and no query_vector → the embed-wired handler rejects the
        // empty query before any network/embed call.
        let out = dispatch("rag_chunk", ResourceOp::Search, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("'q'"));
    }

    #[tokio::test]
    async fn rag_chunk_delete_without_filter_errors_cleanly() {
        // prune requires at least one filter — the adapter forwards an
        // empty args object and the handler surfaces the clean error.
        let out = dispatch("rag_chunk", ResourceOp::Delete, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("filter"));
    }

    #[tokio::test]
    async fn rag_execute_unknown_action_errors_cleanly() {
        let out = dispatch(
            "rag",
            ResourceOp::Execute,
            ResourceRequest { action: Some("bogus".into()), ..Default::default() },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["index", "reindex"]));
    }

    #[tokio::test]
    async fn rag_execute_missing_action_errors_cleanly() {
        let out = dispatch("rag", ResourceOp::Execute, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["index", "reindex"]));
    }

    #[tokio::test]
    async fn rag_feedback_missing_query_id_errors_cleanly() {
        let out = dispatch("rag_feedback", ResourceOp::Create, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("query_id"));
    }

    #[tokio::test]
    async fn rag_feedback_resource_id_supplies_query_id() {
        // query_id via the verb's resource_id, score in body. The handler
        // then complains about score range / records — either way it does
        // NOT complain about a missing query_id.
        let out = dispatch(
            "rag_feedback",
            ResourceOp::Create,
            ResourceRequest {
                resource_id: Some("q-123".into()),
                body: json!({"score": 1}),
                ..Default::default()
            },
        )
        .await;
        // Either ok (recorded) or an error that is NOT "query_id required".
        if out["status"] == "error" {
            assert!(!out["error"].as_str().unwrap().contains("'query_id' is required"));
        }
    }
}
