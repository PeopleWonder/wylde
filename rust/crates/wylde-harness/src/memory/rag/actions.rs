//! Model-callable handlers for the eight `rag.*` tools.
//!
//! Thin glue over the [`super::store`] / [`super::search`] /
//! [`super::miss_log`] / [`super::prune`] / [`super::feedback`] /
//! [`super::ingest`] modules. Each handler returns the envelope shape
//! the corresponding Python tool returns — same field names, same
//! status strings — so a future parity test can compare verbatim.
//!
//! ## Why `rag_ask` looks small
//!
//! The Python `rag_ask` drives `rag_pipeline.ask`, which runs an
//! LLM-decomposition + HyDE + multi-hop + cross-encoder rerank loop on
//! top of the bare vector search. None of that lands in Phase 7.B-3 —
//! the Rust port stops at the bare `search` surface, gated by the
//! caller supplying a precomputed `query_vector` (no embedder is wired
//! into the harness yet — that comes with the wylde-ollama Rust port).
//! See `super::search` module docs for the documented divergence.
//!
//! When `query_vector` is absent the handler returns
//! `status=insufficient_context` with an explanatory note, matching
//! the Python tool's "gate fired" terminal state — a planner can still
//! react to it the same way.
//!
//! ## Why `rag_index` / `rag_reindex` look small
//!
//! Both trigger an N8N webhook via [`super::ingest::trigger_ingest`].
//! The current ingest stub is **transport-deferred** (no `reqwest`
//! integration this slice) — the handler surfaces the webhook URL the
//! call would have hit, so the model sees a clear "use Python harness
//! for now" message instead of silent failure.

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use super::feedback::record_outcome;
use super::ingest::{trigger_ingest, IngestRequest};
use super::miss_log;
use super::prune::{prune_rows, PruneError, PruneFilters};
use super::search::{search_logged, SearchError};
use super::store::TieredStore;
use super::tiers::is_known_tier;
use crate::memory::common::embed_dim;
use crate::memory::memgraph::Client;

// ─── rag.ask ──────────────────────────────────────────────────────────

/// Handler for `rag.ask` / `rag_ask`.
///
/// Accepted args:
/// * `q` (required) — natural-language question.
/// * `query_vector` (optional, array of numbers) — precomputed embedding.
///   When absent the handler returns `insufficient_context` because the
///   Rust embedder isn't wired yet.
/// * `limit` (optional, default 8) — clamped to 1..=50.
/// * `workspace` (optional) — workspace id; advisory in this slice.
/// * `tier` (optional) — restrict search to one tier.
pub async fn run_rag_ask(args: Value) -> Result<Value, IpcError> {
    let query = args
        .get("q")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if query.is_empty() {
        return Ok(json!({
            "status": "error",
            "error": "'q' parameter required and must be non-empty",
        }));
    }
    let limit = clamp_usize(args.get("limit"), 8, 1, 50);
    let workspace_id = args
        .get("workspace")
        .or_else(|| args.get("workspace_id"))
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let tier = args
        .get("tier")
        .or_else(|| args.get("memory_type"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let Some(query_vector) = parse_float_array(args.get("query_vector")) else {
        // No embedder wired — record a synthetic miss so analytics still see the query.
        let _ = miss_log::log_query(&query, &workspace_id, &[], tier.as_deref());
        return Ok(json!({
            "status": "insufficient_context",
            "q": query,
            "results": [],
            "count": 0,
            "reason": "embed_not_wired",
            "note": "Rust rag.ask requires a precomputed `query_vector` until the wylde-ollama \
                     embedder is ported. Use the Python harness for question-only retrieval.",
        }));
    };

    if let Some(t) = &tier {
        if !is_known_tier(t) {
            return Ok(json!({
                "status": "error",
                "error": format!("unknown tier '{t}'"),
            }));
        }
    }

    let store = TieredStore::open_at(&crate::memory::common::data_dir(), embed_dim());
    match search_logged(
        &store,
        &query,
        query_vector,
        tier.as_deref(),
        &workspace_id,
        limit,
    ) {
        Ok(hits) => {
            let results: Vec<Value> = hits.iter().map(|h| h.to_value()).collect();
            let count = results.len();
            for h in &hits {
                miss_log::record_chunk_use(&h.id);
            }
            let status = if count == 0 {
                "insufficient_context"
            } else {
                "ok"
            };
            Ok(json!({
                "status": status,
                "q": query,
                "workspace_id": workspace_id,
                "results": results,
                "count": count,
            }))
        }
        Err(SearchError::UnknownTier(t)) => Ok(json!({
            "status": "error",
            "error": format!("unknown tier '{t}'"),
        })),
        Err(SearchError::Vector(msg)) => Ok(json!({
            "status": "error",
            "error": format!("vector store: {msg}"),
        })),
    }
}

// ─── rag.index ────────────────────────────────────────────────────────

pub async fn run_rag_index(args: Value) -> Result<Value, IpcError> {
    let target_path = args
        .get("target_path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(default_target_path);
    let workspace_id = args
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let paths = parse_string_array(args.get("paths"));
    let force = args
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut options = std::collections::HashMap::new();
    options.insert("force".to_owned(), json!(force));
    options.insert("mode".to_owned(), json!("index"));

    let req = IngestRequest {
        target_path: target_path.clone(),
        workspace_id: workspace_id.clone(),
        paths: if paths.is_empty() { None } else { Some(paths) },
        options: Some(options),
    };
    let result = trigger_ingest(req).await;
    Ok(json!({
        "status": if result["ok"].as_bool().unwrap_or(false) { "ok" } else { "deferred" },
        "target_path": target_path,
        "workspace_id": workspace_id,
        "force": force,
        "ingest": result,
    }))
}

// ─── rag.reindex ──────────────────────────────────────────────────────

pub async fn run_rag_reindex(args: Value) -> Result<Value, IpcError> {
    let target_path = args
        .get("target_path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(default_target_path);
    let workspace_id = args
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();

    let mut options = std::collections::HashMap::new();
    options.insert("force".to_owned(), json!(true));
    options.insert("mode".to_owned(), json!("reindex"));

    let req = IngestRequest {
        target_path: target_path.clone(),
        workspace_id: workspace_id.clone(),
        paths: None,
        options: Some(options),
    };
    let result = trigger_ingest(req).await;
    Ok(json!({
        "status": if result["ok"].as_bool().unwrap_or(false) { "ok" } else { "deferred" },
        "target_path": target_path,
        "workspace_id": workspace_id,
        "ingest": result,
    }))
}

// ─── rag.prune ────────────────────────────────────────────────────────

pub async fn run_rag_prune(args: Value) -> Result<Value, IpcError> {
    let confirm = args
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_delete = clamp_usize(args.get("max_delete"), 500, 1, 10000);
    let filters = PruneFilters {
        before_ts: args.get("before_ts").and_then(Value::as_f64),
        memory_type: args
            .get("memory_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        score_lt: args
            .get("score_lt")
            .and_then(Value::as_f64)
            .map(|x| x as f32),
    };

    if filters.is_empty() {
        return Ok(json!({
            "status": "error",
            "error": "at least one filter required: before_ts, memory_type, or score_lt",
        }));
    }

    let mut store = TieredStore::open_at(&crate::memory::common::data_dir(), embed_dim());

    if !confirm {
        match super::prune::preview(&store, &filters, max_delete) {
            Ok(ids) => Ok(super::prune::dry_run_envelope(ids.len(), &filters, max_delete)),
            Err(PruneError::NoFilter) => Ok(json!({
                "status": "error",
                "error": "at least one filter required: before_ts, memory_type, or score_lt",
            })),
        }
    } else {
        match prune_rows(&mut store, &filters, max_delete) {
            Ok((deleted, ids)) => {
                let _ = store.save();
                Ok(super::prune::ok_envelope(deleted, &ids, &filters))
            }
            Err(PruneError::NoFilter) => Ok(json!({
                "status": "error",
                "error": "at least one filter required: before_ts, memory_type, or score_lt",
            })),
        }
    }
}

// ─── rag.feedback ─────────────────────────────────────────────────────

pub async fn run_rag_feedback(args: Value) -> Result<Value, IpcError> {
    let query_id = args
        .get("query_id")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
                .or_else(|| v.as_f64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    if query_id.is_empty() {
        return Ok(json!({
            "status": "error",
            "error": "'query_id' is required",
        }));
    }
    let score = args
        .get("score")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MIN);
    if score == i64::MIN {
        return Ok(json!({
            "status": "error",
            "error": "'score' is required (number in {-1, 0, 1})",
        }));
    }
    if !(-1..=1).contains(&score) {
        return Ok(json!({
            "status": "error",
            "error": "'score' must be -1, 0, or 1",
        }));
    }
    let comment = args.get("comment").and_then(Value::as_str);
    match miss_log::record_feedback(&query_id, score as i32, comment) {
        Ok(ok) => Ok(json!({
            "status": "ok",
            "recorded": ok,
            "query_id": query_id,
            "score": score,
        })),
        Err(e) => Ok(json!({
            "status": "error",
            "error": e,
        })),
    }
}

// ─── rag.misses ───────────────────────────────────────────────────────

pub async fn run_rag_misses(args: Value) -> Result<Value, IpcError> {
    let limit = clamp_usize(args.get("limit"), 100, 1, 1000);
    let _only_gated = args
        .get("only_gated")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let _include_trace = args
        .get("include_trace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let since = args.get("since").and_then(Value::as_f64);
    let rows = miss_log::list_misses(since, limit);
    Ok(json!({
        "status": "ok",
        "count": rows.len(),
        "rows": rows,
    }))
}

// ─── rag.chunk_usage ──────────────────────────────────────────────────

pub async fn run_rag_chunk_usage(args: Value) -> Result<Value, IpcError> {
    let limit = clamp_usize(args.get("limit"), 100, 1, 10000);
    let dead_only = args
        .get("dead_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let rows = miss_log::chunk_usage(limit);
    let filtered: Vec<Value> = if dead_only {
        rows.into_iter()
            .filter(|r| r.get("count").and_then(Value::as_i64) == Some(0))
            .collect()
    } else {
        rows
    };
    Ok(json!({
        "status": "ok",
        "count": filtered.len(),
        "rows": filtered,
    }))
}

// ─── rag.graph_stats ──────────────────────────────────────────────────

pub async fn run_rag_graph_stats(_args: Value) -> Result<Value, IpcError> {
    run_rag_graph_stats_with_client(&Client::new()).await
}

/// Test seam — same handler but takes an explicit `Client`. The async
/// surface is identical so the tool registry calls `run_rag_graph_stats`
/// directly while unit tests inject a mock transport.
pub async fn run_rag_graph_stats_with_client(client: &Client) -> Result<Value, IpcError> {
    let reply = client.stats().await;
    if !reply.ok {
        return Ok(json!({
            "status": "ok",
            "reachable": false,
            "entities": 0,
            "chunks": 0,
            "mentions": 0,
        }));
    }
    let data = &reply.data;
    Ok(json!({
        "status": "ok",
        "reachable": true,
        "entities": data.get("entities").and_then(Value::as_i64).unwrap_or(0),
        "chunks": data.get("chunks").and_then(Value::as_i64).unwrap_or(0),
        "mentions": data.get("mentions").and_then(Value::as_i64).unwrap_or(0),
    }))
}

// ─── meta.graph_query hybrid feedback hook ────────────────────────────

/// Convenience wrapper around [`record_outcome`] so the hybrid
/// `meta.graph_query` path (and future RAG pipeline) can fold graph
/// feedback in one call. Returns the trace envelope; never errors.
pub async fn record_terminal_outcome(
    client: &Client,
    query: &str,
    status: &str,
    query_entities: &[String],
    chunk_ids: &[String],
    query_id: &str,
) -> Value {
    record_outcome(client, query, status, query_entities, chunk_ids, query_id)
        .await
        .to_value()
}

// ─── helpers ──────────────────────────────────────────────────────────

fn clamp_usize(v: Option<&Value>, fallback: usize, lo: usize, hi: usize) -> usize {
    let raw = v.and_then(Value::as_i64).unwrap_or(fallback as i64);
    raw.max(lo as i64).min(hi as i64) as usize
}

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

fn default_target_path() -> String {
    std::env::var("WYLDE_WORKSPACE_ROOT")
        .or_else(|_| std::env::var("WYLDE_ROOT"))
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memgraph::client::mock;
    use crate::memory::rag::store::TierRecord;
    use crate::memory::rag::test_support::TestEnv;
    use wylde_shared::ipc::Reply;

    fn data_dir_for_test() -> std::path::PathBuf {
        std::env::var_os("WYLDE_DATA_DIR")
            .map(std::path::PathBuf::from)
            .expect("TestEnv binds WYLDE_DATA_DIR")
    }

    fn seed_store() -> TieredStore {
        let mut s = TieredStore::open_at(&data_dir_for_test(), 4);
        s.insert(
            TierRecord::new("a", "alpha body", "episodic", 0.4, "", ""),
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        s.insert(
            TierRecord::new("b", "beta body", "core", 0.9, "", ""),
            Some(vec![0.0, 1.0, 0.0, 0.0]),
        )
        .unwrap();
        s.save().unwrap();
        s
    }

    fn force_embed_dim_4() {
        std::env::set_var("WYLDE_EMBED_DIM", "4");
    }

    // ── rag_ask ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rag_ask_requires_q() {
        let _env = TestEnv::new();
        let v = run_rag_ask(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn rag_ask_without_vector_returns_insufficient_context() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_ask(json!({"q": "anything"})).await.unwrap();
        assert_eq!(v["status"], "insufficient_context");
        assert_eq!(v["reason"], "embed_not_wired");
        assert_eq!(v["count"], 0);
    }

    #[tokio::test]
    async fn rag_ask_with_vector_returns_ranked_hits() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let _store = seed_store();
        let v = run_rag_ask(json!({
            "q": "alpha",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
            "limit": 5,
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "ok");
        let results = v["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["id"], "a");
    }

    #[tokio::test]
    async fn rag_ask_unknown_tier_returns_error() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let _store = seed_store();
        let v = run_rag_ask(json!({
            "q": "x",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
            "tier": "junk",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn rag_ask_empty_store_returns_insufficient_context_status() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_ask(json!({
            "q": "anything",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "insufficient_context");
        assert_eq!(v["count"], 0);
    }

    // ── rag_index / rag_reindex ────────────────────────────────────────

    #[tokio::test]
    async fn rag_index_surfaces_deferred_status_when_n8n_unreachable() {
        let _env = TestEnv::new();
        // Pin to an unreachable URL so the handler exercises its error
        // path. Successful transport is covered in ingest::tests against
        // a wiremock server.
        std::env::set_var("WYLDE_N8N_BASE_URL", "http://127.0.0.1:1");
        std::env::set_var("WYLDE_N8N_INGEST_TIMEOUT_S", "1");
        let v = run_rag_index(json!({
            "target_path": "/tmp/repo",
            "workspace_id": "ws-1",
            "paths": ["a", "b"],
            "force": true,
        }))
        .await
        .unwrap();
        // ingest.ok=false → handler surfaces status=deferred (matches the
        // pre-Seam-4 wire contract; only the underlying error code changed).
        assert_eq!(v["status"], "deferred");
        assert_eq!(v["workspace_id"], "ws-1");
        assert_eq!(v["force"], true);
        let err = v["ingest"]["error"].as_str().unwrap_or("");
        assert!(
            err == "connect_failed" || err == "request_failed",
            "expected connect_failed/request_failed, got {err}"
        );
        std::env::remove_var("WYLDE_N8N_BASE_URL");
        std::env::remove_var("WYLDE_N8N_INGEST_TIMEOUT_S");
    }

    #[tokio::test]
    async fn rag_reindex_forces_force_flag_in_ingest_options() {
        let _env = TestEnv::new();
        std::env::set_var("WYLDE_N8N_BASE_URL", "http://127.0.0.1:1");
        std::env::set_var("WYLDE_N8N_INGEST_TIMEOUT_S", "1");
        let v = run_rag_reindex(json!({"workspace_id": "ws-99"})).await.unwrap();
        assert_eq!(v["status"], "deferred");
        assert_eq!(v["workspace_id"], "ws-99");
        std::env::remove_var("WYLDE_N8N_BASE_URL");
        std::env::remove_var("WYLDE_N8N_INGEST_TIMEOUT_S");
    }

    // ── rag_prune ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn rag_prune_requires_at_least_one_filter() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_prune(json!({"confirm": true})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn rag_prune_dry_run_returns_count_only() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let _store = seed_store();
        let v = run_rag_prune(json!({"memory_type": "episodic"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "dry_run");
        assert_eq!(v["would_delete"], 1);
    }

    #[tokio::test]
    async fn rag_prune_confirm_actually_deletes() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let _store = seed_store();
        let v = run_rag_prune(json!({
            "memory_type": "episodic",
            "confirm": true,
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["deleted"], 1);
        // Reload from disk — confirm persistence.
        let reloaded = TieredStore::open_at(&data_dir_for_test(), 4);
        assert_eq!(reloaded.count_rows(), 1);
    }

    // ── rag_feedback ───────────────────────────────────────────────────

    #[tokio::test]
    async fn rag_feedback_requires_query_id() {
        let _env = TestEnv::new();
        let v = run_rag_feedback(json!({"score": 1})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn rag_feedback_rejects_out_of_range_score() {
        let _env = TestEnv::new();
        let v = run_rag_feedback(json!({"query_id": "q1", "score": 2}))
            .await
            .unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn rag_feedback_records_valid_payload() {
        let _env = TestEnv::new();
        let v = run_rag_feedback(json!({"query_id": "q1", "score": 1, "comment": "helpful"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["recorded"], true);
    }

    #[tokio::test]
    async fn rag_feedback_accepts_numeric_query_id() {
        // Python's manifest declares query_id as a number; we accept both.
        let _env = TestEnv::new();
        let v = run_rag_feedback(json!({"query_id": 42, "score": 0}))
            .await
            .unwrap();
        assert_eq!(v["status"], "ok");
    }

    // ── rag_misses ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn rag_misses_returns_recorded_misses() {
        let _env = TestEnv::new();
        miss_log::log_query("missed", "ws", &[], None);
        miss_log::log_query("also-missed", "ws", &[], None);
        let v = run_rag_misses(json!({"limit": 10})).await.unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 2);
    }

    // ── rag_chunk_usage ────────────────────────────────────────────────

    #[tokio::test]
    async fn rag_chunk_usage_returns_counter_rows() {
        let _env = TestEnv::new();
        miss_log::record_chunk_use("c1");
        miss_log::record_chunk_use("c1");
        miss_log::record_chunk_use("c2");
        let v = run_rag_chunk_usage(json!({"limit": 100}))
            .await
            .unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 2);
        // First row is the highest-count chunk.
        assert_eq!(v["rows"][0]["chunk_id"], "c1");
    }

    #[tokio::test]
    async fn rag_chunk_usage_dead_only_filters_zero_counts() {
        let _env = TestEnv::new();
        miss_log::record_chunk_use("alive");
        let v = run_rag_chunk_usage(json!({"dead_only": true}))
            .await
            .unwrap();
        // No counter is ever 0 in our store, so dead_only returns empty.
        assert_eq!(v["count"], 0);
    }

    // ── rag_graph_stats ────────────────────────────────────────────────

    #[tokio::test]
    async fn rag_graph_stats_reports_reachable_when_service_replies_ok() {
        let (client, _) = mock::new_with_static_ok(json!({
            "entities": 12,
            "chunks": 34,
            "mentions": 56,
        }));
        let v = run_rag_graph_stats_with_client(&client).await.unwrap();
        assert_eq!(v["reachable"], true);
        assert_eq!(v["entities"], 12);
        assert_eq!(v["chunks"], 34);
        assert_eq!(v["mentions"], 56);
    }

    #[tokio::test]
    async fn rag_graph_stats_reports_unreachable_on_pipe_error() {
        let (client, _) =
            mock::new_with_responder(|_| Reply::err_msg("pipe_connect", "no service"));
        let v = run_rag_graph_stats_with_client(&client).await.unwrap();
        assert_eq!(v["reachable"], false);
        assert_eq!(v["entities"], 0);
        assert_eq!(v["chunks"], 0);
        assert_eq!(v["mentions"], 0);
    }

    // ── record_terminal_outcome ────────────────────────────────────────

    #[tokio::test]
    async fn record_terminal_outcome_returns_trace_envelope() {
        let _env = TestEnv::new();
        let (client, _) = mock::new_with_static_ok(json!({"updated": true}));
        let trace =
            record_terminal_outcome(&client, "q", "ok", &["foo".into()], &["chunk-1".into()], "qid")
                .await;
        assert_eq!(trace["graph_ok"], true);
        assert_eq!(trace["graph_edges"], 1);
    }
}
