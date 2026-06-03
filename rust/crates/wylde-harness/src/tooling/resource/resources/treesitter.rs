//! Tree-sitter resources (tool-registry consolidation Slice 3,
//! `docs/plans/tool-registry-consolidation.md` §5).
//!
//! Exposes the `wylde-treesitter` sidecar's structural-parsing surface
//! under the generic verbs:
//!
//! | Resource | Op | Sidecar action |
//! |---|---|---|
//! | `code_chunk` | list | `treesitter.chunk` (AST-boundary-aware chunking of a file) |
//! | `code_entity` | list | `treesitter.extract_entities` (functions/classes/imports/calls) |
//!
//! ## Cross-process — IPC, not in-process
//!
//! Unlike the memory / RAG resources (whose handlers live in this crate),
//! the tree-sitter handlers live in a **separate OS process** — the
//! `wylde-treesitter` sidecar serving `\\.\pipe\wylde-treesitter` (and a
//! loopback HTTP front door for N8N). The [`OpHandler`]s therefore do one
//! `ipc::call_action(cfg.treesitter_service, "treesitter.<verb>", …)` hop,
//! exactly the path the extension-bridge resources (Slice 5a) use. The
//! service name is configurable ([`crate::config::Config::treesitter_service`])
//! so tests can retarget a fake pipe.
//!
//! ## Why `list`, not `search` — and no semantic search
//!
//! `treesitter.chunk` / `treesitter.extract_entities` are **path-keyed
//! enumerations**: given a file path they return that file's chunks /
//! entities. That is a `list` (enumerate the parts of one file), not a
//! `search` (rank across a corpus). The sidecar has **no semantic-search
//! verb** — semantic retrieval over code lives in the RAG layer
//! (`rag_chunk` search, embeddings). Per the plan's "don't build resources
//! on missing handlers" rule, these resources expose only `list`; no
//! `search` op is registered because no such handler exists. The plan §5
//! sketch (`wylde_list("chunk", filter={path, language})`) is honoured
//! exactly.
//!
//! ## Read-only
//!
//! Parsing a file mutates nothing — both resources are non-destructive at
//! every op, so the coarse `wylde_list` gate and the fine per-op gate both
//! pass straight through.

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

/// Register the tree-sitter resources into the built-in registry.
pub fn register_treesitter_resources(reg: &mut ResourceRegistry) {
    register_code_chunk(reg);
    register_code_entity(reg);
}

// ── code_chunk — AST-boundary-aware chunking of one file ─────────────

fn register_code_chunk(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(op_chunk_list));

    reg.register_builtin(ResourceDefinition {
        resource_type: "code_chunk",
        display_name: "Code chunk",
        description: "AST-boundary-aware chunks of one source file (functions/classes; byte \
                      windows for unknown languages). Enumerate by file path via the \
                      wylde-treesitter sidecar.",
        scope: Scope::Global,
        identifier_fields: &["path"],
        filter_fields: &["path", "language", "max_chunk_bytes"],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_code_chunk),
    });
}

/// `wylde_list("code_chunk", {path, language?, max_chunk_bytes?})` →
/// `treesitter.chunk`. `path` may also arrive as the verb's `resource_id`.
fn op_chunk_list(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let payload = build_path_payload(&req, &["path", "language", "max_chunk_bytes"]);
    async move { call_sidecar(cfg, "treesitter.chunk", "code_chunk", payload).await }
}

// ── code_entity — structural entities of one file ────────────────────

fn register_code_entity(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(op_entity_list));

    reg.register_builtin(ResourceDefinition {
        resource_type: "code_entity",
        display_name: "Code entity",
        description: "Structural entities of one source file — functions, classes (with \
                      methods/bases), imports, and call edges. Feeds the Memgraph graph layer. \
                      Enumerate by file path via the wylde-treesitter sidecar.",
        scope: Scope::Global,
        identifier_fields: &["path"],
        filter_fields: &["path", "language"],
        operations,
        destructive_ops: &[],
        describe: describe_value(describe_code_entity),
    });
}

/// `wylde_list("code_entity", {path, language?})` →
/// `treesitter.extract_entities`.
fn op_entity_list(
    req: ResourceRequest,
    cfg: &'static Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let payload = build_path_payload(&req, &["path", "language"]);
    async move { call_sidecar(cfg, "treesitter.extract_entities", "code_entity", payload).await }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Build the sidecar payload from the verb's `filter`, keeping only the
/// keys the action accepts, and injecting `path` from `resource_id` when
/// the model passed the file there instead of inside `filter`.
fn build_path_payload(req: &ResourceRequest, allowed: &[&str]) -> Value {
    let filter = match &req.filter {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let mut out = Map::new();
    for key in allowed {
        if let Some(v) = filter.get(*key) {
            out.insert((*key).to_owned(), v.clone());
        }
    }
    if !out.contains_key("path") {
        if let Some(id) = &req.resource_id {
            out.insert("path".into(), json!(id));
        }
    }
    Value::Object(out)
}

/// One `ipc::call_action` hop to the sidecar. On success the reply data is
/// returned with a `status: "ok"` stamped in (the sidecar replies don't
/// carry one); a failed call — including the sidecar being down — is
/// folded into a clean `status: "error"` envelope rather than propagated
/// as a hard `Err`, so a missing sidecar never aborts the turn.
async fn call_sidecar(
    cfg: &Config,
    action: &str,
    resource_type: &str,
    payload: Value,
) -> Result<Value, IpcError> {
    // Guard: path is required by both sidecar verbs. Reject before the IPC
    // hop so a missing path is a clean local error, not a pipe round-trip.
    if payload.get("path").and_then(Value::as_str).map(str::trim).unwrap_or("").is_empty() {
        return Ok(json!({
            "status": "error",
            "resource_type": resource_type,
            "error": format!(
                "wylde_list(\"{resource_type}\", …) requires a 'path' (in filter or as resource_id)"
            ),
        }));
    }

    match ipc::call_action(&cfg.treesitter_service, action, payload).await {
        Ok(v) => Ok(stamp_ok(v)),
        Err(e) => Ok(json!({
            "status": "error",
            "resource_type": resource_type,
            "error": e.message,
            "code": e.code,
            "hint": "is the wylde-treesitter sidecar running? (pipe \\\\.\\pipe\\wylde-treesitter)",
        })),
    }
}

/// Stamp `status: "ok"` onto a sidecar reply object (idempotent). Non-object
/// replies are wrapped so callers always get a uniform envelope.
fn stamp_ok(v: Value) -> Value {
    match v {
        Value::Object(mut m) => {
            m.entry("status").or_insert_with(|| json!("ok"));
            Value::Object(m)
        }
        other => json!({"status": "ok", "result": other}),
    }
}

// ── describe() ───────────────────────────────────────────────────────

fn describe_code_chunk() -> Value {
    json!({
        "resource_type": "code_chunk",
        "display_name": "Code chunk",
        "description": "AST-boundary-aware chunks of one source file.",
        "scope": "global",
        "identifier_fields": ["path"],
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "Chunk one file at AST boundaries (treesitter.chunk). Reply: \
                                {chunks:[{start_line,end_line,byte_start,byte_end,kind,symbol_name?}], ast_aware}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File path to chunk (required)"},
                                "language": {"type": "string", "description": "Override language (inferred from extension when omitted)"},
                                "max_chunk_bytes": {"type": "number", "description": "Max bytes per chunk"}
                            },
                            "required": ["path"]
                        }
                    }
                }
            }
        }
    })
}

fn describe_code_entity() -> Value {
    json!({
        "resource_type": "code_entity",
        "display_name": "Code entity",
        "description": "Structural entities of one source file.",
        "scope": "global",
        "identifier_fields": ["path"],
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "Extract entities from one file (treesitter.extract_entities). \
                                Reply: {functions:[{name,line}], classes:[{name,line,methods,bases}], \
                                imports:[{module,line}], calls:[{caller,callee,line}], module, counts}.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File path to parse (required)"},
                                "language": {"type": "string", "description": "Override language (inferred from extension when omitted)"}
                            },
                            "required": ["path"]
                        }
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
        register_treesitter_resources(&mut r);
        r
    }

    fn cfg() -> &'static Config {
        Box::leak(Box::new(Config::default_for_tests()))
    }

    #[test]
    fn registers_two_code_resources() {
        let r = reg();
        assert!(r.lookup("code_chunk").is_some());
        assert!(r.lookup("code_entity").is_some());
        assert_eq!(r.builtin_len(), 2);
    }

    #[test]
    fn both_are_list_only_and_non_destructive() {
        let r = reg();
        for rt in ["code_chunk", "code_entity"] {
            let def = r.lookup(rt).unwrap();
            assert_eq!(def.supported_ops(), vec![ResourceOp::List], "{rt} ops");
            assert!(!def.is_destructive(ResourceOp::List), "{rt} destructive");
            // No search op — semantic search is not a sidecar capability.
            assert!(!def.supports(ResourceOp::Search), "{rt} must not expose search");
        }
    }

    #[test]
    fn neither_is_searchable() {
        let r = reg();
        assert!(r.searchable_types(&ToolsetFilter::all()).is_empty());
    }

    #[test]
    fn build_path_payload_filters_to_allowed_keys() {
        let req = ResourceRequest {
            filter: json!({"path": "a.py", "language": "python", "bogus": 1, "max_chunk_bytes": 2000}),
            ..Default::default()
        };
        let p = build_path_payload(&req, &["path", "language", "max_chunk_bytes"]);
        assert_eq!(p["path"], "a.py");
        assert_eq!(p["language"], "python");
        assert_eq!(p["max_chunk_bytes"], 2000);
        assert!(p.get("bogus").is_none(), "unknown keys are dropped");
    }

    #[test]
    fn build_path_payload_lifts_path_from_resource_id() {
        let req = ResourceRequest {
            resource_id: Some("b.rs".into()),
            ..Default::default()
        };
        let p = build_path_payload(&req, &["path", "language"]);
        assert_eq!(p["path"], "b.rs");
    }

    #[test]
    fn stamp_ok_is_idempotent_and_wraps_non_objects() {
        let already = stamp_ok(json!({"status": "ok", "chunks": []}));
        assert_eq!(already["status"], "ok");
        let stamped = stamp_ok(json!({"chunks": []}));
        assert_eq!(stamped["status"], "ok");
        let wrapped = stamp_ok(json!([1, 2, 3]));
        assert_eq!(wrapped["status"], "ok");
        assert_eq!(wrapped["result"], json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn list_without_path_errors_before_any_ipc() {
        // No path anywhere → clean local error, no pipe round-trip (which
        // would otherwise fail with a connect error against a dead pipe).
        let r = reg();
        let def = r.lookup("code_chunk").unwrap();
        let handler = def.operations.get(&ResourceOp::List).unwrap().clone();
        let out = handler
            .call(ResourceRequest::default(), cfg(), ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn describe_exposes_only_list() {
        for d in [describe_code_chunk(), describe_code_entity()] {
            let ops = d["operations"].as_object().unwrap();
            assert!(ops.contains_key("list"));
            assert!(!ops.contains_key("search"));
            assert_eq!(ops["list"]["destructive"], false);
        }
    }
}
