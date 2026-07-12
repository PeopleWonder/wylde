//! `memory.*` — active long-term memory tools (Phase 7.B).
//!
//! Four model-callable tools wired against
//! [`crate::memory::long_term`]:
//!
//! * `memory_long_term_save` — store a new record.
//! * `memory_update` — revise an existing record (writes a new version
//!   and supersedes the old).
//! * `memory_delete` — permanently remove a record.
//! * `memory_search` — vector + recency-decay search.
//!
//! Workspace-scoped memory is ALSO exposed here now (the
//! `memory_workspace_*` tools) — save / update / delete / search / list,
//! each wired straight through to the `memory.workspace.*` action
//! handlers in [`crate::memory::workspace::actions`]. This closes the
//! locked-design drift where the model could reach the durable
//! workspace tier only over the pipe, not as a tool-call. RAG tools are
//! a separate parallel subtask.
//!
//! ## Embeddings
//!
//! Save / update take an optional `vector` parameter. When the caller
//! supplies one it is mirrored verbatim; when it is absent the handler
//! now embeds the body itself via [`crate::memory::embeddings`]
//! (budgeted + fail-soft — see [`crate::memory::embed_write`]), so the
//! `long_term.vec.bin` mirror stays populated without the caller having
//! to pre-embed. The workspace save/update handlers do the same against
//! their per-workspace `memory.vec.bin` mirror.
//!
//! Search accepts EITHER a `query` (string — preferred; embedded via
//! [`crate::memory::embeddings`]) OR a precomputed `query_vector`. The
//! string path is the canonical post-Phase-9-cleanup surface; the
//! precomputed-vector path is kept for callers that already have an
//! embedding in hand (e.g. tests, or reuse across a batch of searches).

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};

use crate::memory::long_term;
use crate::memory::workspace::actions as ws_actions;
use crate::tooling::registry::{entry_active, param, param_default, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "memory_long_term_save",
        "memory.long_term.save",
        "memory",
        "Save a long-term memory that should persist across conversations \
         and workspaces. Optionally include a precomputed `vector` to mirror \
         into the vector index.",
        vec![
            param("body", "string", true, "Memory text"),
            param_default("source", "string", "Origin tag", json!("")),
            param_default("importance", "number", "Importance 0..10", json!(null)),
            param_default("tags", "array", "Optional tag list", json!([])),
            param_default("vector", "array", "Precomputed embedding", json!(null)),
        ],
        true,
        |args, _| async move { run_save(args).await },
    ));

    reg.insert(entry_active(
        "memory_update",
        "memory.update",
        "memory",
        "Revise an existing memory. Writes a new version and supersedes \
         the old one (the prior body stays visible via the history walker).",
        vec![
            param("memory_id", "string", true, "Memory id"),
            param_default("body", "string", "New body (optional)", json!(null)),
            param_default("importance", "number", "New importance", json!(null)),
            param_default("source", "string", "New source tag", json!(null)),
            param_default("vector", "array", "Precomputed embedding", json!(null)),
        ],
        true,
        |args, _| async move { run_update(args).await },
    ));

    reg.insert(entry_active(
        "memory_delete",
        "memory.delete",
        "memory",
        "Permanently remove a memory and anything superseded by it.",
        vec![param("memory_id", "string", true, "Memory id")],
        true,
        |args, _| async move { run_delete(args).await },
    ));

    reg.insert(entry_active(
        "memory_search",
        "memory.search",
        "memory",
        "Vector + recency-decay search over long-term memory. Pass \
         either a `query` string (embedded via wylde-ollama) or a \
         precomputed `query_vector`. Superseded records are filtered \
         out; results are ranked by similarity boosted by importance + \
         recency decay.",
        vec![
            param_default(
                "query",
                "string",
                "Text query (embedded via wylde-ollama)",
                json!(null),
            ),
            param_default(
                "query_vector",
                "array",
                "Precomputed embedding (alternative to `query`)",
                json!(null),
            ),
            param_default("limit", "number", "Max hits to return", json!(5)),
            param_default(
                "decay_days",
                "number",
                "Recency decay constant",
                json!(30.0),
            ),
            // Kept for shape parity with the deferred catalog entry that
            // existed before this slice — currently advisory only.
            param_default(
                "scope",
                "string",
                "Scope (must be 'long_term')",
                json!("long_term"),
            ),
        ],
        false,
        |args, _| async move { run_search(args).await },
    ));

    // ── Workspace-scoped tier ────────────────────────────────────────
    //
    // The durable middle memory tier (memory plan M2, option B): the
    // turn gather injects a workspace's top-k as the `### Workspace
    // insights` prompt slot, so a model save here really does resurface
    // in later workspace-bound turns. These mirror the long-term surface
    // (save / update / delete / search) plus `list`, and each delegates
    // straight through to the `memory.workspace.*` action handlers in
    // [`crate::memory::workspace::actions`] — one implementation, two
    // surfaces (named tool + pipe verb).

    reg.insert(entry_active(
        "memory_workspace_save",
        "memory.workspace.save",
        "memory",
        "Save a memory scoped to a workspace. The workspace's top memories \
         are injected into later prompts for that workspace, so this is how \
         you durably teach yourself about a project. Requires `workspace_id`.",
        vec![
            param("workspace_id", "string", true, "Target workspace id"),
            param("body", "string", true, "Memory text"),
            param_default("source", "string", "Origin tag", json!("")),
            param_default("importance", "number", "Importance 0..10", json!(null)),
            param_default("entities", "array", "Entity names for graph edges", json!([])),
        ],
        true,
        |args, _| async move { reply_to_value(ws_actions::handle_save(args).await) },
    ));

    reg.insert(entry_active(
        "memory_workspace_update",
        "memory.workspace.update",
        "memory",
        "Revise a workspace memory. Writes a new version and supersedes the \
         old one. Requires `workspace_id` and `id`.",
        vec![
            param("workspace_id", "string", true, "Workspace id"),
            param("id", "string", true, "Memory id to revise"),
            param_default("body", "string", "New body (optional)", json!(null)),
            param_default("importance", "number", "New importance", json!(null)),
            param_default("entities", "array", "Replacement entity list", json!(null)),
        ],
        true,
        |args, _| async move { reply_to_value(ws_actions::handle_update(args).await) },
    ));

    reg.insert(entry_active(
        "memory_workspace_delete",
        "memory.workspace.delete",
        "memory",
        "Permanently remove a workspace memory (and its superseded \
         predecessors). Requires `workspace_id` and `id`.",
        vec![
            param("workspace_id", "string", true, "Workspace id"),
            param("id", "string", true, "Memory id"),
        ],
        true,
        |args, _| async move { reply_to_value(ws_actions::handle_delete(args).await) },
    ));

    reg.insert(entry_active(
        "memory_workspace_search",
        "memory.workspace.search",
        "memory",
        "Search a workspace's memories, ranked by relevance boosted by \
         importance + recency decay. Requires `workspace_id` and `query`.",
        vec![
            param("workspace_id", "string", true, "Workspace id"),
            param("query", "string", true, "Text query"),
            param_default("limit", "number", "Max hits (1..=50, default 5)", json!(5)),
        ],
        false,
        |args, _| async move { reply_to_value(ws_actions::handle_search(args).await) },
    ));

    reg.insert(entry_active(
        "memory_workspace_list",
        "memory.workspace.list",
        "memory",
        "List every memory for a workspace, importance then recency \
         ordered. Requires `workspace_id`.",
        vec![
            param("workspace_id", "string", true, "Workspace id"),
            param_default(
                "include_superseded",
                "boolean",
                "Include superseded / tombstoned records",
                json!(false),
            ),
        ],
        false,
        |args, _| async move { reply_to_value(ws_actions::handle_list(args).await) },
    ));
}

/// Adapt a workspace-action [`Reply`] into the `{status, …}` value shape
/// the named `memory.*` tools return. Success merges `status: "success"`
/// into the reply's data object (so `id` / `hits` / `count` stay
/// top-level); failure surfaces the `error` message + `code`, matching
/// the long-term handlers' error envelope.
fn reply_to_value(reply: Reply) -> Result<Value, IpcError> {
    if reply.ok {
        let value = match reply.data {
            Value::Object(mut m) => {
                m.insert("status".to_owned(), json!("success"));
                Value::Object(m)
            }
            other => json!({ "status": "success", "data": other }),
        };
        Ok(value)
    } else {
        let (error, code) = match reply.error {
            Some(e) => (e.message, e.code),
            None => ("workspace memory operation failed".to_owned(), String::new()),
        };
        Ok(json!({ "status": "error", "error": error, "code": code }))
    }
}

// ── Handlers ─────────────────────────────────────────────────────────
//
// These are `pub(crate)` so the verb layer's memory `OpHandler`s
// (`tooling/resource/resources/memory.rs`, consolidation Slice 2) can
// delegate into them rather than duplicate the logic — the verb tools
// adapt their `ResourceRequest` into the `args` shape these expect and
// call straight through. The named-tool registrations above are
// unchanged; both surfaces share one implementation.

pub(crate) async fn run_save(args: Value) -> Result<Value, IpcError> {
    let Some(body) = args.get("body").and_then(Value::as_str) else {
        return Ok(json!({"status": "error", "error": "'body' is required"}));
    };
    let source = args.get("source").and_then(Value::as_str).unwrap_or("");
    let importance = args.get("importance").and_then(Value::as_f64);
    let tags = parse_string_array(args.get("tags"));
    // Caller-supplied vector wins; otherwise embed the body ourselves so
    // the `long_term.vec.bin` mirror stays populated (budgeted, fail-soft).
    let vector = match parse_float_array(args.get("vector")) {
        Some(v) => Some(v),
        None => crate::memory::embed_write::embed_for_write(body).await,
    };
    match long_term::save(body, source, importance, tags, vector) {
        Ok(r) => Ok(json!({
            "status": "success",
            "id": r.id,
            "body": r.body,
            "importance": r.importance,
            "created_at": r.created_at,
        })),
        Err(e) => Ok(json!({
            "status": "error",
            "error": e.to_string(),
        })),
    }
}

pub(crate) async fn run_update(args: Value) -> Result<Value, IpcError> {
    let Some(memory_id) = args.get("memory_id").and_then(Value::as_str) else {
        return Ok(json!({"status": "error", "error": "'memory_id' is required"}));
    };
    let body = args.get("body").and_then(Value::as_str);
    let importance = args.get("importance").and_then(Value::as_f64);
    let source = args.get("source").and_then(Value::as_str);
    // The replacement record's mirror vector: caller-supplied wins;
    // otherwise embed the effective new body (the supplied `body`, or the
    // original's body when the update leaves it unchanged) so the mirror
    // tracks the current text. Budgeted + fail-soft.
    let vector = match parse_float_array(args.get("vector")) {
        Some(v) => Some(v),
        None => {
            let effective_body = body
                .map(str::to_owned)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| long_term::get(memory_id).map(|r| r.body));
            match effective_body {
                Some(text) => crate::memory::embed_write::embed_for_write(&text).await,
                None => None,
            }
        }
    };
    match long_term::update(memory_id, body, importance, source, vector) {
        Some(r) => Ok(json!({
            "status": "success",
            "id": r.id,
            "body": r.body,
            "importance": r.importance,
            "created_at": r.created_at,
        })),
        None => Ok(json!({
            "status": "error",
            "error": format!("memory not found: {memory_id}"),
            "code": "not_found",
        })),
    }
}

pub(crate) async fn run_delete(args: Value) -> Result<Value, IpcError> {
    let Some(memory_id) = args.get("memory_id").and_then(Value::as_str) else {
        return Ok(json!({"status": "error", "error": "'memory_id' is required"}));
    };
    let deleted = long_term::delete(memory_id);
    if deleted {
        Ok(json!({"status": "success", "id": memory_id}))
    } else {
        Ok(json!({
            "status": "error",
            "error": format!("memory not found: {memory_id}"),
            "code": "not_found",
        }))
    }
}

pub(crate) async fn run_search(args: Value) -> Result<Value, IpcError> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
    let decay = args.get("decay_days").and_then(Value::as_f64);

    // Precomputed-vector path takes precedence when both are present —
    // it sidesteps the IPC hop entirely. Otherwise route through the
    // embedder.
    if let Some(query_vector) = parse_float_array(args.get("query_vector")) {
        let hits = long_term::search(query_vector, limit, decay);
        return Ok(json!({
            "status": "success",
            "results": hits.iter().map(|h| h.to_value()).collect::<Vec<_>>(),
        }));
    }

    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    if query.trim().is_empty() {
        return Ok(json!({
            "status": "error",
            "error": "either 'query' (string) or 'query_vector' (array of numbers) is required",
        }));
    }
    match long_term::text_search(query, limit, decay).await {
        Ok(hits) => Ok(json!({
            "status": "success",
            "results": hits.iter().map(|h| h.to_value()).collect::<Vec<_>>(),
        })),
        Err(long_term::TextSearchError::EmptyQuery) => Ok(json!({
            "status": "error",
            "error": "query is empty after trim",
        })),
        Err(long_term::TextSearchError::Embed(e)) => Ok(json!({
            "status": "error",
            "error": format!("embed failed: {e}"),
        })),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn parse_string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_float_array(v: Option<&Value>) -> Option<Vec<f32>> {
    let arr = v?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let out: Option<Vec<f32>> = arr.iter().map(|x| x.as_f64().map(|n| n as f32)).collect();
    out.filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;

    fn set_embed_dim_3() {
        std::env::set_var("WYLDE_EMBED_DIM", "3");
    }

    #[tokio::test]
    async fn save_handler_persists_and_returns_id() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v = run_save(json!({
            "body": "hello",
            "source": "ui",
            "importance": 7,
            "tags": ["a"],
            "vector": [1.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["importance"], 7);
        let id = v["id"].as_str().unwrap();
        assert!(long_term::get(id).is_some());
    }

    #[tokio::test]
    async fn save_handler_errors_when_body_missing() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v = run_save(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn save_handler_errors_when_body_empty_after_trim() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v = run_save(json!({"body": "   "})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn update_handler_supersedes_original() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let saved = run_save(json!({"body": "v1", "importance": 5}))
            .await
            .unwrap();
        let orig_id = saved["id"].as_str().unwrap().to_owned();

        let v = run_update(json!({
            "memory_id": orig_id,
            "body": "v2",
            "importance": 8,
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["importance"], 8);
        let new_id = v["id"].as_str().unwrap();
        assert_ne!(new_id, orig_id);

        // Original now points at the replacement.
        let orig = long_term::get(&orig_id).unwrap();
        assert_eq!(orig.superseded_by, new_id);
    }

    #[tokio::test]
    async fn update_handler_unknown_id_returns_not_found() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v = run_update(json!({"memory_id": "ghost", "body": "x"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["code"], "not_found");
    }

    #[tokio::test]
    async fn delete_handler_removes_record() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let saved = run_save(json!({"body": "doomed", "importance": 5}))
            .await
            .unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        let v = run_delete(json!({"memory_id": id.clone()})).await.unwrap();
        assert_eq!(v["status"], "success");
        assert!(long_term::get(&id).is_none());
    }

    #[tokio::test]
    async fn search_handler_returns_results_for_known_vector() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        run_save(json!({
            "body": "near",
            "importance": 6,
            "vector": [1.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        let v = run_search(json!({
            "query_vector": [1.0, 0.0, 0.0],
            "limit": 5,
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        let results = v["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["body"], "near");
    }

    #[tokio::test]
    async fn search_handler_errors_when_query_vector_missing() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v = run_search(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn memory_tools_register_under_canonical_and_alias_keys() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("memory_long_term_save").is_some());
        assert!(reg.lookup("memory.long_term.save").is_some());
        assert!(reg.lookup("memory_update").is_some());
        assert!(reg.lookup("memory.update").is_some());
        assert!(reg.lookup("memory_delete").is_some());
        assert!(reg.lookup("memory_search").is_some());
    }

    #[tokio::test]
    async fn workspace_memory_tools_register_under_canonical_and_alias_keys() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for (id, dotted) in [
            ("memory_workspace_save", "memory.workspace.save"),
            ("memory_workspace_update", "memory.workspace.update"),
            ("memory_workspace_delete", "memory.workspace.delete"),
            ("memory_workspace_search", "memory.workspace.search"),
            ("memory_workspace_list", "memory.workspace.list"),
        ] {
            assert_eq!(reg.lookup(id).map(|e| e.id.clone()).as_deref(), Some(id));
            assert_eq!(reg.lookup(dotted).map(|e| e.id.clone()).as_deref(), Some(id));
        }
        // Write ops destructive; read ops not.
        assert!(reg.lookup("memory_workspace_save").unwrap().destructive);
        assert!(reg.lookup("memory_workspace_update").unwrap().destructive);
        assert!(reg.lookup("memory_workspace_delete").unwrap().destructive);
        assert!(!reg.lookup("memory_workspace_search").unwrap().destructive);
        assert!(!reg.lookup("memory_workspace_list").unwrap().destructive);
    }

    #[tokio::test]
    async fn workspace_tool_adapter_maps_success_and_error_envelopes() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // Success: status merged in, record fields stay top-level.
        let ok = reply_to_value(
            ws_actions::handle_save(json!({
                "workspace_id": "wt", "body": "a durable note", "importance": 6,
            }))
            .await,
        )
        .unwrap();
        assert_eq!(ok["status"], "success");
        assert!(ok["id"].as_str().is_some());
        assert_eq!(ok["workspace_id"], "wt");

        // List round-trips the saved record via the tool adapter.
        let listed =
            reply_to_value(ws_actions::handle_list(json!({"workspace_id": "wt"})).await).unwrap();
        assert_eq!(listed["status"], "success");
        assert_eq!(listed["count"], 1);

        // Missing workspace_id → error envelope with code preserved.
        let err =
            reply_to_value(ws_actions::handle_save(json!({"body": "orphan"})).await).unwrap();
        assert_eq!(err["status"], "error");
        assert_eq!(err["code"], "bad_request");
        assert_eq!(err["error"], "workspace_id is required");
    }

    #[tokio::test]
    async fn destructive_tools_marked_destructive_search_is_not() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("memory_long_term_save").unwrap().destructive);
        assert!(reg.lookup("memory_update").unwrap().destructive);
        assert!(reg.lookup("memory_delete").unwrap().destructive);
        assert!(!reg.lookup("memory_search").unwrap().destructive);
    }
}
