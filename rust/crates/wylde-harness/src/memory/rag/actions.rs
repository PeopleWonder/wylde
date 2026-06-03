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
use super::store::{TierRecord, TieredStore};
use super::tiers::{is_known_tier, TIER_EPISODIC};
use crate::memory::common::{data_dir, embed_dim};
use crate::memory::embeddings::{embed_one, EmbedError};
use crate::memory::memgraph::current_traversal_impl;
use crate::memory::memgraph::transport::MemgraphTraversal;

/// Default episodic score when the caller doesn't supply one. Episodic
/// rows are mid-tier importance — below `core` (1.0), above a cold
/// `semantic` chunk. Matches the seed convention used across the
/// `rag` test fixtures.
const DEFAULT_EPISODIC_SCORE: f32 = 0.5;

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

// ─── rag.add_episodic (Wylde_Study S2a) ───────────────────────────────

/// Handler for the `rag.add_episodic` pipe action.
///
/// Raw-text episodic write — the Rust port of Python
/// `Core/harness/memory/rag.add_episodic(body, source_path, session_id)`,
/// which is in turn a thin wrapper over `vector_store.add_row(memory_type
/// = EPISODIC, …)`. Lands one `episodic`-tier row in the same
/// [`TieredStore`] that [`run_rag_search`] / [`run_rag_ask`] read, so an
/// added page is immediately retrievable.
///
/// Accepted args:
/// * `content` / `text` (required) — the episodic body (the caller
///   composes any title prefix itself, exactly as the Python handler does).
/// * `source_path` / `url` (optional) — origin trace stored on the row so
///   a later hit links back to its page.
/// * `session_id` (optional) — grouping tag.
/// * `score` (optional) — episodic importance; defaults to
///   [`DEFAULT_EPISODIC_SCORE`].
/// * `vector` (optional, array of numbers) — precomputed embedding. When
///   absent the body is embedded server-side via [`embed_one`] (same
///   embedder the rest of the memory layer uses). Mirrors the
///   precomputed-vector escape hatch the `memory` resource and `rag.ask`
///   already expose, so tests and pre-embedding callers don't need a live
///   wylde-ollama.
///
/// Returns `{status: "ok", memory_id, id, chars, memory_type}` on
/// success — `memory_id` matches the Python handler's field name, `id`
/// the `rag.*` family convention; both carry the same value.
pub async fn run_rag_add_episodic(args: Value) -> Result<Value, IpcError> {
    let content = args
        .get("content")
        .or_else(|| args.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if content.is_empty() {
        return Ok(json!({
            "status": "error",
            "error": "'content' (or 'text') is required and must be non-empty",
        }));
    }
    let source_path = args
        .get("source_path")
        .or_else(|| args.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let score = args
        .get("score")
        .and_then(Value::as_f64)
        .map(|s| s as f32)
        .unwrap_or(DEFAULT_EPISODIC_SCORE);

    let provided = parse_float_array(args.get("vector"));
    let vector = match resolve_vector(provided, &content).await {
        Ok(v) => v,
        Err(e) => return Ok(embed_error_envelope(e)),
    };

    let id = uuid::Uuid::new_v4().simple().to_string();
    let record = TierRecord::new(
        id.clone(),
        content.clone(),
        TIER_EPISODIC,
        score,
        session_id,
        source_path,
    );

    let mut store = TieredStore::open_at(&data_dir(), embed_dim());
    if let Err(e) = store.insert(record, Some(vector)) {
        return Ok(json!({
            "status": "error",
            "error": format!("vector store: {e}"),
        }));
    }
    if let Err(e) = store.save() {
        return Ok(json!({
            "status": "error",
            "error": format!("persist failed: {e}"),
        }));
    }

    Ok(json!({
        "status": "ok",
        "memory_id": id,
        "id": id,
        "chars": content.chars().count(),
        "memory_type": TIER_EPISODIC,
    }))
}

// ─── rag.search (Wylde_Study S2a) ─────────────────────────────────────

/// Handler for the `rag.search` pipe action.
///
/// The embed-wired sibling of [`run_rag_ask`]: where `rag.ask` deliberately
/// refuses to embed (it's the model-callable tool and returns
/// `embed_not_wired` without a precomputed vector), `rag.search` is the
/// extension-facing action that *does* embed the query server-side via
/// [`embed_one`], then runs the exact same first-party search
/// ([`search_logged`] over [`TieredStore`]). No parallel retrieval path —
/// only the query-embedding step differs.
///
/// Accepted args:
/// * `q` (required) — natural-language query.
/// * `query_vector` (optional, array of numbers) — precomputed embedding;
///   skips the embed round-trip when supplied (test / pre-embed seam).
/// * `limit` (optional, default 8) — clamped to 1..=50.
/// * `tier` (optional) — restrict to one tier (`core` / `episodic` / …).
/// * `workspace` / `workspace_id` (optional) — advisory, for miss-log
///   attribution.
///
/// Returns the same envelope shape as `rag.ask`
/// (`{status, q, workspace_id, results, count}`) so the two verbs are
/// interchangeable on the read side.
pub async fn run_rag_search(args: Value) -> Result<Value, IpcError> {
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
    if let Some(t) = &tier {
        if !is_known_tier(t) {
            return Ok(json!({
                "status": "error",
                "error": format!("unknown tier '{t}'"),
            }));
        }
    }

    let provided = parse_float_array(args.get("query_vector"));
    let query_vector = match resolve_vector(provided, &query).await {
        Ok(v) => v,
        Err(e) => return Ok(embed_error_envelope(e)),
    };

    let store = TieredStore::open_at(&data_dir(), embed_dim());
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
    // Honor the strangler selection (Bolt by default) like the rest of
    // the graph path — the previous hardcoded pipe `Client` reached the
    // `\\.\pipe\wylde-memgraph` surface retired in the 2026-05-26
    // cutover, so this tool always reported `reachable: false`.
    run_rag_graph_stats_with_client(&current_traversal_impl()).await
}

/// Test seam — same handler but takes an explicit traversal client. The
/// async surface is identical so the tool registry calls
/// `run_rag_graph_stats` directly while unit tests inject a mock
/// transport.
pub async fn run_rag_graph_stats_with_client(
    client: &impl MemgraphTraversal,
) -> Result<Value, IpcError> {
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
    client: &impl MemgraphTraversal,
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

/// Use the caller's precomputed vector if present, otherwise embed
/// `text` via [`embed_one`] — the single embedder the whole memory
/// layer shares. Keeps `rag.add_episodic` / `rag.search` from growing a
/// second embedding codepath.
async fn resolve_vector(provided: Option<Vec<f32>>, text: &str) -> Result<Vec<f32>, EmbedError> {
    match provided {
        Some(v) => Ok(v),
        None => embed_one(text.to_owned()).await,
    }
}

/// Shape an [`EmbedError`] into the `status: error` envelope the
/// `rag.*` family uses. Distinct `code` so a caller can tell an embed
/// failure (transient backend / model missing) from a bad-args error.
fn embed_error_envelope(e: EmbedError) -> Value {
    json!({
        "status": "error",
        "code": "embed_failed",
        "error": e.to_string(),
    })
}

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

    // ── rag_add_episodic (Wylde_Study S2a) ─────────────────────────────

    #[tokio::test]
    async fn add_episodic_requires_content() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_add_episodic(json!({"url": "http://x"})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn add_episodic_rejects_blank_content() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_add_episodic(json!({"content": "   "})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn add_episodic_with_precomputed_vector_persists_episodic_row() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_add_episodic(json!({
            "content": "mitochondria are the powerhouse of the cell",
            "url": "http://bio.example/cell",
            "session_id": "s-1",
            "vector": [1.0, 0.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["memory_type"], "episodic");
        assert_eq!(v["chars"], 43);
        let id = v["memory_id"].as_str().expect("memory_id present");
        assert_eq!(v["id"], id, "id and memory_id carry the same value");

        // Reload from disk → the row is an episodic tier record with the
        // url stored in source_path.
        let store = TieredStore::open_at(&data_dir_for_test(), 4);
        assert_eq!(store.count_rows(), 1);
        let row = store.iter().next().unwrap();
        assert_eq!(row.memory_type, "episodic");
        assert_eq!(row.source_path, "http://bio.example/cell");
        assert_eq!(row.session_id, "s-1");
    }

    #[tokio::test]
    async fn add_episodic_accepts_text_alias_for_content() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_add_episodic(json!({
            "text": "alias body",
            "vector": [0.0, 1.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "ok");
    }

    // ── rag_search (Wylde_Study S2a) ────────────────────────────────────

    #[tokio::test]
    async fn search_requires_q() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_search(json!({"query_vector": [1.0, 0.0, 0.0, 0.0]}))
            .await
            .unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn search_unknown_tier_returns_error() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_search(json!({
            "q": "x",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
            "tier": "junk",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn search_empty_store_returns_insufficient_context() {
        let _env = TestEnv::new();
        force_embed_dim_4();
        let v = run_rag_search(json!({
            "q": "anything",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "insufficient_context");
        assert_eq!(v["count"], 0);
    }

    #[tokio::test]
    async fn add_episodic_then_search_round_trips() {
        // The core integration contract: an episodic row added via
        // run_rag_add_episodic surfaces in a follow-up run_rag_search.
        let _env = TestEnv::new();
        force_embed_dim_4();
        let added = run_rag_add_episodic(json!({
            "content": "the Krebs cycle runs in the mitochondrial matrix",
            "url": "http://bio.example/krebs",
            "vector": [1.0, 0.0, 0.0, 0.0],
        }))
        .await
        .unwrap();
        assert_eq!(added["status"], "ok");
        let id = added["memory_id"].as_str().unwrap().to_owned();

        let found = run_rag_search(json!({
            "q": "what runs in the mitochondria",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
            "limit": 5,
        }))
        .await
        .unwrap();
        assert_eq!(found["status"], "ok");
        let results = found["results"].as_array().unwrap();
        assert!(!results.is_empty(), "added row must surface in search");
        assert_eq!(results[0]["id"], id);
        assert_eq!(results[0]["memory_type"], "episodic");
        assert_eq!(
            results[0]["content"],
            "the Krebs cycle runs in the mitochondrial matrix"
        );
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
