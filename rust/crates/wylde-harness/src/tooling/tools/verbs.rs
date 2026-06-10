//! The eight verb tools — `wylde_describe / list / get / create /
//! update / delete / search / execute`. Tool-registry consolidation
//! Slice 1 (`docs/plans/tool-registry-consolidation.md`).
//!
//! Each verb is a normal [`crate::tooling::registry::ToolEntry`]. It
//! flows through the **unchanged** `dispatch_tool` → tier gate → consent
//! gate pipeline (so it co-exists with the old named tools — both are
//! advertised in the catalog this slice); the handler then resolves
//! `resource_type` + the verb's [`ResourceOp`] and dispatches through the
//! [`super::super::resource::ResourceRegistry`].
//!
//! ## Coarse vs fine gating
//!
//! Each verb carries a **coarse** `destructive` flag so the existing
//! runner gates behave correctly today: read verbs (`describe`, `list`,
//! `get`, `search`) are non-destructive; mutating verbs (`create`,
//! `update`, `delete`, `execute`) are destructive. After the op
//! resolves, [`super::super::resource::op_consent_gate`] applies the
//! **fine** per-`(resource, op)` refinement — only when the coarse verb
//! gate did not already prompt, so there is never a double prompt
//! (`resource/gate.rs`).
//!
//! ## Slice 1 behaviour
//!
//! The resource registry is empty, so:
//! * `wylde_describe` (no arg) returns an empty resource list;
//! * `wylde_describe(resource_type="x")` / every other verb returns a
//!   clean `not_found` envelope.
//!
//! Real resources arrive in Slice 2+.

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::config::Config;
use crate::tooling::registry::{entry_active, param, param_default, Registry};
use crate::tooling::resource::{
    op_consent_gate, resources, OpGate, ResourceOp, ResourceRequest, ToolContext, ToolsetFilter,
};

/// Register all eight verb tools into the per-tool catalog.
pub fn register(reg: &mut Registry) {
    // ── describe — local discovery, no side effects, no resource op ──
    reg.insert(entry_active(
        "wylde_describe",
        "wylde_describe",
        "verbs",
        "Discover Wylde resources. With no argument, returns a compact \
         list of every resource type you can operate on (its display \
         name, supported verbs, and scope). With `resource_type`, returns \
         that resource's full self-description (identifier/filter fields \
         and per-op notes). Call this first to learn what `resource_type` \
         values the other verb tools accept — the legal values are not \
         in the always-on prompt.",
        vec![
            param(
                "resource_type",
                "string",
                false,
                "Resource to describe; omit for the full list",
            ),
            param_default(
                "compact",
                "boolean",
                "Compact rows only (default true)",
                json!(true),
            ),
        ],
        false,
        |args, _cfg| async move { run_describe(args) },
    ));

    // ── read verbs (coarse non-destructive) ─────────────────────────
    reg.insert(entry_active(
        "wylde_list",
        "wylde_list",
        "verbs",
        "List instances of a resource. Args: resource_type (required), \
         optional filter object, limit, cursor. Use wylde_describe to see \
         a resource's filter fields.",
        vec![
            param(
                "resource_type",
                "string",
                true,
                "Resource type, e.g. 'memory', 'file'",
            ),
            param(
                "filter",
                "object",
                false,
                "Filter predicate (resource-specific)",
            ),
            param("limit", "number", false, "Max rows"),
            param("cursor", "string", false, "Pagination cursor"),
        ],
        false,
        |args, cfg| async move { run_verb(ResourceOp::List, false, args, cfg).await },
    ));

    reg.insert(entry_active(
        "wylde_get",
        "wylde_get",
        "verbs",
        "Fetch one instance of a resource by id. Args: resource_type \
         (required), resource_id.",
        vec![
            param("resource_type", "string", true, "Resource type"),
            param("resource_id", "string", false, "Identifier of the instance"),
        ],
        false,
        |args, cfg| async move { run_verb(ResourceOp::Get, false, args, cfg).await },
    ));

    reg.insert(entry_active(
        "wylde_search",
        "wylde_search",
        "verbs",
        "Search a resource (or '*' to fan out across every search-capable \
         resource). Args: resource_type (required; '*' for all), query, \
         optional filter, limit.",
        vec![
            param(
                "resource_type",
                "string",
                true,
                "Resource type, or '*' for all searchable",
            ),
            param("query", "string", false, "Free-text query"),
            param("filter", "object", false, "Filter predicate"),
            param("limit", "number", false, "Max results"),
        ],
        false,
        |args, cfg| async move { run_search(args, cfg).await },
    ));

    // ── mutating verbs (coarse destructive) ─────────────────────────
    reg.insert(entry_active(
        "wylde_create",
        "wylde_create",
        "verbs",
        "Create a new instance of a resource. Args: resource_type \
         (required), body object.",
        vec![
            param("resource_type", "string", true, "Resource type"),
            param(
                "body",
                "object",
                false,
                "Creation payload (resource-specific)",
            ),
        ],
        true,
        |args, cfg| async move { run_verb(ResourceOp::Create, true, args, cfg).await },
    ));

    reg.insert(entry_active(
        "wylde_update",
        "wylde_update",
        "verbs",
        "Update an existing instance of a resource. Args: resource_type \
         (required), resource_id, body object.",
        vec![
            param("resource_type", "string", true, "Resource type"),
            param("resource_id", "string", false, "Identifier of the instance"),
            param("body", "object", false, "Fields to change"),
        ],
        true,
        |args, cfg| async move { run_verb(ResourceOp::Update, true, args, cfg).await },
    ));

    reg.insert(entry_active(
        "wylde_delete",
        "wylde_delete",
        "verbs",
        "Delete an instance (by resource_id) or matching instances (by \
         filter). Args: resource_type (required), resource_id OR filter.",
        vec![
            param("resource_type", "string", true, "Resource type"),
            param("resource_id", "string", false, "Identifier to delete"),
            param("filter", "object", false, "Predicate for bulk delete"),
        ],
        true,
        |args, cfg| async move { run_verb(ResourceOp::Delete, true, args, cfg).await },
    ));

    reg.insert(entry_active(
        "wylde_execute",
        "wylde_execute",
        "verbs",
        "Run a named action on a resource (the verb for operations that \
         aren't plain CRUD). Args: resource_type (required), action \
         (required), optional params object.",
        vec![
            param("resource_type", "string", true, "Resource type"),
            param(
                "action",
                "string",
                false,
                "Sub-action selector, e.g. 'preload'",
            ),
            param("params", "object", false, "Action parameters"),
        ],
        true,
        |args, cfg| async move { run_verb(ResourceOp::Execute, true, args, cfg).await },
    ));
}

/// `wylde_describe` — local metadata, no resource op, no side effects.
fn run_describe(args: Value) -> Result<Value, IpcError> {
    let filter = ToolsetFilter::all();
    let reg = resources();

    match args.get("resource_type").and_then(Value::as_str) {
        Some(rt) if !rt.is_empty() => match reg.lookup_visible(rt, &filter) {
            Some(def) => Ok(json!({
                "status": "success",
                "resource_type": rt,
                "definition": def.describe(),
            })),
            None => Ok(json!({
                "status": "not_found",
                "resource_type": rt,
                "error": format!("no resource type {rt:?} is registered"),
                "available": reg
                    .summary_rows(&filter)
                    .iter()
                    .filter_map(|r| r["resource_type"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>(),
            })),
        },
        _ => Ok(json!({
            "status": "success",
            "resources": reg.summary_rows(&filter),
            "count": reg.summary_rows(&filter).len(),
        })),
    }
}

/// Shared dispatch for the single-resource verbs (everything except
/// describe and search-with-`*`).
async fn run_verb(
    op: ResourceOp,
    coarse_destructive: bool,
    args: Value,
    cfg: &'static Config,
) -> Result<Value, IpcError> {
    let filter = ToolsetFilter::all();
    let reg = resources();

    let Some(rt) = args
        .get("resource_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(missing_resource_type(op));
    };

    let Some(def) = reg.lookup_visible(rt, &filter) else {
        return Ok(not_found(rt, op, reg, &filter));
    };

    let Some(handler) = def.operations.get(&op).cloned() else {
        return Ok(unsupported_op(rt, op, &def));
    };

    // Fine per-(resource, op) consent refinement — a no-op when the
    // coarse verb gate already handled consent or the op is read-only.
    let effective_destructive = def.is_destructive(op);
    if let OpGate::Block { error, .. } =
        op_consent_gate(rt, op, effective_destructive, coarse_destructive)
    {
        return Err(error);
    }

    let req = ResourceRequest::from_args(op, &args);
    let ctx = ToolContext::for_op(rt, op, req.resource_id.clone());
    handler.call(req, cfg, ctx).await
}

/// `wylde_search` — resolves `resource_type`, with `"*"` fanning out
/// across every search-capable resource.
async fn run_search(args: Value, cfg: &'static Config) -> Result<Value, IpcError> {
    let rt = args
        .get("resource_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    if rt == "*" {
        let filter = ToolsetFilter::all();
        let reg = resources();
        let types = reg.searchable_types(&filter);
        let mut results: Vec<Value> = Vec::new();
        for t in &types {
            if let Some(def) = reg.lookup_visible(t, &filter) {
                if let Some(handler) = def.operations.get(&ResourceOp::Search).cloned() {
                    let req = ResourceRequest::from_args(ResourceOp::Search, &args);
                    let ctx = ToolContext::for_op(t, ResourceOp::Search, None);
                    match handler.call(req, cfg, ctx).await {
                        Ok(v) => results.push(json!({"resource_type": t, "result": v})),
                        Err(e) => results.push(json!({"resource_type": t, "error": e.message})),
                    }
                }
            }
        }
        return Ok(json!({
            "status": "success",
            "fanout": true,
            "searched": types,
            "results": results,
        }));
    }

    run_verb(ResourceOp::Search, false, args, cfg).await
}

// ── error envelopes ─────────────────────────────────────────────────

fn missing_resource_type(op: ResourceOp) -> Value {
    json!({
        "status": "error",
        "error": format!(
            "wylde_{} requires a 'resource_type' argument; call \
             wylde_describe to list valid resource types",
            op.as_str()
        ),
    })
}

fn not_found(
    rt: &str,
    op: ResourceOp,
    reg: &crate::tooling::resource::ResourceRegistry,
    filter: &ToolsetFilter,
) -> Value {
    json!({
        "status": "not_found",
        "resource_type": rt,
        "op": op.as_str(),
        "error": format!(
            "no resource type {rt:?} is registered; call wylde_describe \
             to list valid resource types"
        ),
        "available": reg
            .summary_rows(filter)
            .iter()
            .filter_map(|r| r["resource_type"].as_str().map(str::to_owned))
            .collect::<Vec<_>>(),
    })
}

fn unsupported_op(
    rt: &str,
    op: ResourceOp,
    def: &crate::tooling::resource::ResourceDefinition,
) -> Value {
    json!({
        "status": "unsupported_op",
        "resource_type": rt,
        "op": op.as_str(),
        "error": format!(
            "resource {rt:?} does not support the {:?} op",
            op.as_str()
        ),
        "supported_ops": def.supported_ops().iter().map(|o| o.as_str()).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::registry::Registry;

    fn cfg() -> &'static Config {
        Box::leak(Box::new(Config::default_for_tests()))
    }

    #[test]
    fn register_adds_eight_verbs() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "wylde_describe",
            "wylde_list",
            "wylde_get",
            "wylde_create",
            "wylde_update",
            "wylde_delete",
            "wylde_search",
            "wylde_execute",
        ] {
            assert!(reg.lookup(id).is_some(), "missing verb {id}");
        }
        assert_eq!(reg.len(), 8);
    }

    #[test]
    fn verb_destructive_flags_are_coarse_correct() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for (id, want) in [
            ("wylde_describe", false),
            ("wylde_list", false),
            ("wylde_get", false),
            ("wylde_search", false),
            ("wylde_create", true),
            ("wylde_update", true),
            ("wylde_delete", true),
            ("wylde_execute", true),
        ] {
            let e = reg.lookup(id).unwrap();
            assert_eq!(e.destructive, want, "{id} destructive flag");
        }
    }

    #[test]
    fn describe_lists_registered_resources() {
        // Slice 2 lights up the `memory` resource in the global registry,
        // so the no-arg describe now lists it (was empty in Slice 1).
        let out = run_describe(json!({})).unwrap();
        assert_eq!(out["status"], "success");
        let types: Vec<&str> = out["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["resource_type"].as_str().unwrap())
            .collect();
        assert!(
            types.contains(&"memory"),
            "describe should list memory; got {types:?}"
        );
        assert_eq!(out["count"].as_u64().unwrap(), types.len() as u64);
    }

    #[test]
    fn describe_memory_returns_full_definition() {
        let out = run_describe(json!({"resource_type": "memory"})).unwrap();
        assert_eq!(out["status"], "success");
        assert_eq!(out["resource_type"], "memory");
        assert_eq!(out["definition"]["resource_type"], "memory");
        assert!(out["definition"]["operations"]["search"].is_object());
    }

    #[test]
    fn describe_unknown_resource_returns_not_found() {
        let out = run_describe(json!({"resource_type": "nope"})).unwrap();
        assert_eq!(out["status"], "not_found");
        assert_eq!(out["resource_type"], "nope");
    }

    #[tokio::test]
    async fn list_unknown_resource_returns_not_found() {
        let out = run_verb(
            ResourceOp::List,
            false,
            json!({"resource_type": "nope"}),
            cfg(),
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "not_found");
        assert_eq!(out["op"], "list");
    }

    #[tokio::test]
    async fn verb_without_resource_type_errors_cleanly() {
        let out = run_verb(ResourceOp::Get, false, json!({}), cfg())
            .await
            .unwrap();
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("resource_type"));
    }

    #[test]
    fn verbs_are_advertised_in_catalog_injection() {
        // Requirement #4: the verb tools must be visible in the catalog
        // injection that turn/prompt.rs does. Post-Slice-6 the verbs are
        // the always-on surface — advertised in both modes.
        let mut reg = Registry::empty();
        register(&mut reg);
        let catalog = crate::tooling::runner::catalog_payload(&reg);
        for verb_mode in [false, true] {
            let prompt = crate::turn::prompt::build_system_prompt(&catalog, verb_mode);
            let tools = crate::turn::prompt::build_tools_field(&catalog, verb_mode);
            for id in [
                "wylde_describe",
                "wylde_list",
                "wylde_search",
                "wylde_delete",
            ] {
                assert!(
                    prompt.contains(id),
                    "system prompt should advertise {id} (verb_mode={verb_mode})"
                );
                assert!(
                    tools.iter().any(|t| t["function"]["name"] == id),
                    "native tools field should advertise {id} (verb_mode={verb_mode})"
                );
            }
        }
    }

    /// Slice 6 cutover — the model-facing catalog built from the **full**
    /// default registry must contain exactly the eight verb tools plus the
    /// surviving named-tool tail when verb mode is on, and strictly more
    /// when off. The exact number is asserted so a future tool addition
    /// that forgets to classify itself (retire vs survive) trips this test.
    #[test]
    fn cutover_catalog_is_exactly_verbs_plus_survivors() {
        use crate::tooling::registry::Registry;
        let reg = Registry::default(); // the full catalog the harness ships
        let catalog = crate::tooling::runner::catalog_payload(&reg);

        let before = crate::turn::prompt::build_tools_field(&catalog, false);
        let after = crate::turn::prompt::build_tools_field(&catalog, true);

        // Post-Slice-4b: 8 verbs + 4 surviving named tools (the imperative
        // voice device triggers only). The former 11 "awaiting-migration"
        // tools (ollama×4, time×2, diff×1, voice transcribe/synthesize×4)
        // are now resource-backed and retired. See docs/wylde-phase6-cutover.md.
        assert_eq!(
            after.len(),
            12,
            "verb-mode catalog size changed: {:?}",
            after
                .iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            before.len() > after.len(),
            "cutover must shrink the catalog: before={} after={}",
            before.len(),
            after.len()
        );

        // Every advertised tool is either a verb or a known survivor —
        // no resource-backed named tool leaked through.
        let names: Vec<String> = after
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
            .collect();
        for retired in [
            "memory.search",
            "rag.ask",
            "meta.graph_query",
            "fs.read_file",
            "search.code_search",
            "meta.tool_search",
            // newly retired in 4b:
            "ollama.list_loaded_models",
            "ollama.evict_model",
            "time.now",
            "time.format",
            "diff.show_diff",
            "voice.transcribe",
            "voice.synthesize_stream",
        ] {
            assert!(
                !names.contains(&retired.to_owned()),
                "{retired} should be retired: {names:?}"
            );
        }
        // Only the verbs and the 4 imperative voice device tools survive.
        for survivor in [
            "wylde_search",
            "wylde_execute",
            "voice.mic.start",
            "voice.mic.stop",
            "voice.wakeword.start",
            "voice.wakeword.stop",
        ] {
            assert!(
                names.contains(&survivor.to_owned()),
                "{survivor} should survive: {names:?}"
            );
        }
    }

    /// Regression: retiring a named tool from the *catalog* must not break
    /// its handler — every retired cluster's operation is still reachable
    /// through its verb resource. This proves the underlying Rust handlers
    /// didn't go anywhere (the cutover is advertising-only).
    #[test]
    fn retired_named_tool_ops_still_reachable_via_verb_resources() {
        let reg = resources();
        let filter = ToolsetFilter::all();
        // (resource_type, op that the retired named tool maps to)
        let pairs = [
            ("memory", ResourceOp::Search),        // memory.search
            ("memory", ResourceOp::Create),        // memory.long_term.save
            ("memory", ResourceOp::Update),        // memory.update
            ("memory", ResourceOp::Delete),        // memory.delete
            ("fs_file", ResourceOp::Get),          // fs.read_file
            ("fs_file", ResourceOp::Update),       // fs.edit_file / fs.write_file
            ("fs_file", ResourceOp::Search),       // search.code_search
            ("fs_dir", ResourceOp::Search),        // search.code_search_files
            ("rag_chunk", ResourceOp::Search),     // rag.ask
            ("rag_chunk", ResourceOp::Delete),     // rag.prune
            ("rag", ResourceOp::Execute),          // rag.index / rag.reindex
            ("rag_feedback", ResourceOp::Create),  // rag.feedback
            ("rag_miss", ResourceOp::List),        // rag.misses
            ("rag_chunk_usage", ResourceOp::List), // rag.chunk_usage
            ("rag_graph_stats", ResourceOp::Get),  // rag.graph_stats
            ("graph", ResourceOp::Search),         // meta.graph_query
            // ── Slice 4b clusters ──
            ("model", ResourceOp::List),    // ollama.list_loaded_models
            ("model", ResourceOp::Create),  // ollama.preload_model
            ("model", ResourceOp::Delete),  // ollama.evict_model
            ("model", ResourceOp::Execute), // ollama.auto_evict_lru
            ("time", ResourceOp::Get),      // time.now
            ("time", ResourceOp::Execute),  // time.format
            ("diff", ResourceOp::Execute),  // diff.show_diff
            ("voice", ResourceOp::Execute), // voice.transcribe / synthesize (+stream)
        ];
        for (rt, op) in pairs {
            let def = reg
                .lookup_visible(rt, &filter)
                .unwrap_or_else(|| panic!("resource {rt} must be registered after cutover"));
            assert!(
                def.operations.contains_key(&op),
                "resource {rt} must still support {op:?} (handler retired by mistake?)"
            );
        }
    }

    #[tokio::test]
    async fn search_star_fans_out_across_searchable_resources() {
        // Slice 2: `memory` is the only searchable resource so far, so the
        // `"*"` fan-out targets it. Drive the precomputed-vector path
        // (query_vector in filter) so no embedder/network is needed.
        let _env = crate::memory::long_term::test_support::TestEnv::new();
        std::env::set_var("WYLDE_EMBED_DIM", "3");
        let out = run_search(
            json!({"resource_type": "*", "filter": {"query_vector": [1.0, 0.0, 0.0]}}),
            cfg(),
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "success");
        assert_eq!(out["fanout"], true);
        let searched: Vec<&str> = out["searched"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(
            searched.contains(&"memory"),
            "fan-out should include memory; got {searched:?}"
        );
    }
}
