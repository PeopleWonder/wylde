//! Knowledge-graph resource — entity / vector hybrid traversal (search).
//!
//! Exposes `wylde_search("graph", …)` over the generic verb surface,
//! delegating to [`crate::memory::memgraph::actions::run_graph_query`]
//! (the same handler the `meta.graph_query` named tool uses, routed
//! through `current_traversal_impl()` so Bolt vs pipe is selected by the
//! strangler flag).
//!
//! ## History
//!
//! This used to live in `resources/rag.rs` alongside six `rag_*`
//! resources backed by the harness `memory/rag/` subsystem. Memory plan
//! **M7** retired that subsystem; the `rag_chunk` / `rag` / `rag_feedback`
//! / `rag_miss` / `rag_chunk_usage` / `rag_graph_stats` resources went
//! with it. The `graph` resource is **not** rag — it is the knowledge-
//! graph entry point M4 made model-reachable (server-side embed of `q`,
//! hybrid vector+graph fusion over the long-term store) — so it survives
//! the deletion in this dedicated module.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;

/// Register the knowledge-graph resource into the built-in registry.
pub fn register_graph_resources(reg: &mut ResourceRegistry) {
    register_graph(reg);
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
        filter_fields: &[
            "entities",
            "query_vector",
            "max_hops",
            "vector_k",
            "workspace_id",
            "limit",
        ],
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
        register_graph_resources(&mut r);
        r
    }

    #[test]
    fn registers_the_graph_resource() {
        let r = reg();
        assert!(r.lookup("graph").is_some(), "missing graph resource");
        assert_eq!(r.builtin_len(), 1);
    }

    #[test]
    fn graph_supports_search_only_and_is_non_destructive() {
        let r = reg();
        let def = r.lookup("graph").unwrap();
        assert_eq!(def.supported_ops(), vec![ResourceOp::Search]);
        assert!(!def.is_destructive(ResourceOp::Search));
    }

    #[test]
    fn graph_is_searchable() {
        let r = reg();
        let types = r.searchable_types(&ToolsetFilter::all());
        assert_eq!(types, vec!["graph".to_string()]);
    }

    #[test]
    fn describe_graph_advertises_hybrid_filter_fields() {
        let v = describe_graph();
        let props = v["operations"]["search"]["schema"]["properties"]["filter"]["properties"]
            .as_object()
            .unwrap();
        assert!(props.contains_key("query_vector"));
        assert!(props.contains_key("entities"));
        // The retired rag-tier param is gone (long-term store is single-tier).
        assert!(!props.contains_key("tier"));
    }
}
