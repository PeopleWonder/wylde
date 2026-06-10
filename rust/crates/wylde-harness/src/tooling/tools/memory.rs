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
//! Workspace-scoped memory (`memory_workspace_save`) stays deferred —
//! the `workspace_memory/` tier hasn't been ported yet (planned for a
//! later 7.B+ slice). RAG tools are a separate parallel subtask.
//!
//! ## Embeddings
//!
//! Save / update take an optional `vector` parameter. When present the
//! vector mirror is updated alongside the JSON record; when absent only
//! the JSON is touched and the next `reindex` pass will rebuild the
//! vector half. This keeps the write-side surface free of an Ollama
//! dependency — the harness's chat loop can embed the body BEFORE
//! calling these tools when an embedding is wanted.
//!
//! Search accepts EITHER a `query` (string — preferred; embedded via
//! [`crate::memory::embeddings`]) OR a precomputed `query_vector`. The
//! string path is the canonical post-Phase-9-cleanup surface; the
//! precomputed-vector path is kept for callers that already have an
//! embedding in hand (e.g. tests, or reuse across a batch of searches).

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::memory::long_term;
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
    let vector = parse_float_array(args.get("vector"));
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
    let vector = parse_float_array(args.get("vector"));
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
    async fn destructive_tools_marked_destructive_search_is_not() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("memory_long_term_save").unwrap().destructive);
        assert!(reg.lookup("memory_update").unwrap().destructive);
        assert!(reg.lookup("memory_delete").unwrap().destructive);
        assert!(!reg.lookup("memory_search").unwrap().destructive);
    }
}
