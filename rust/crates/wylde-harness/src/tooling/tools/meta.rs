//! `meta.*` tools — Rust port of `Core/harness/tooling/tools/meta/`.
//!
//! Phase 6 shipped `tool_search` (in-process catalog discovery against
//! the registry). Phase 7 memgraph port wires `graph_query` to the
//! Rust Memgraph IPC client — entity-driven hybrid retrieval against
//! `wylde-memgraph`.
//!
//! The scoring algorithm is a direct port of the Python `_score_match`
//! function — the comment in `tool_search.py` says "keep the two in
//! lock-step", so we preserve the token-overlap heuristic verbatim.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::registry::{entry_active, param, param_default, Registry};

static TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z][A-Za-z0-9_]+").expect("static token regex"));

static STOPWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let mut s = HashSet::new();
    for w in [
        "the", "a", "an", "is", "are", "to", "for", "of", "in", "on", "at", "and", "or", "with",
        "tool", "find", "i", "need", "want", "that", "does", "do", "can", "use", "uses", "using",
        "this", "any",
    ] {
        s.insert(w);
    }
    s
});

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "tool_search",
        "meta.tool_search",
        "meta",
        "Find Wylde tools by natural-language description. Scores the \
         in-process tool catalog (descriptions, tags, ids) and returns \
         the best matches.",
        vec![
            param("query", "string", true, "Free-form description"),
            param_default("limit", "number", "Max results", json!(5)),
            param(
                "group",
                "string",
                false,
                "Optional group filter (e.g. 'fs', 'memory')",
            ),
        ],
        false,
        |args, _| async move { run_tool_search(args).await },
    ));

    reg.insert(entry_active(
        "graph_query",
        "meta.graph_query",
        "meta",
        "Search the code/knowledge graph with a natural-language query. \
         `q` is embedded server-side and runs the hybrid path — vector \
         top-K seeds plus graph expansion, fused and ranked. Pass \
         `entities` to walk the graph from explicit entity names \
         instead. Fail-soft: if the embedder is unavailable the call \
         degrades to entity-seed traversal extracted from `q`.",
        vec![
            param(
                "q",
                "string",
                false,
                "Natural-language query — embedded server-side for hybrid retrieval",
            ),
            param(
                "entities",
                "array",
                false,
                "Explicit entity-name list; skips embedding and identifier extraction",
            ),
            param(
                "query_vector",
                "array",
                false,
                "Advanced: precomputed embedding (overrides the server-side embed of `q`)",
            ),
            param_default(
                "max_hops",
                "number",
                "Graph expansion depth (1..4)",
                json!(1),
            ),
            param_default("limit", "number", "Max chunks returned (1..50)", json!(10)),
            param_default(
                "vector_k",
                "number",
                "Vector hits to seed expansion with (1..20)",
                json!(5),
            ),
            param(
                "tier",
                "string",
                false,
                "Restrict vector search to one tier (core/episodic/semantic/procedural)",
            ),
            param(
                "workspace_id",
                "string",
                false,
                "Filter chunks to this workspace",
            ),
        ],
        false,
        |args, _| async move {
            // Strangler-fig dispatch — default `rust` (post-2026-05-26
            // cutover) selects the direct-Bolt path;
            // `WYLDE_HARNESS_MEMORY_IMPL=python` is the rollback
            // escape hatch back to the IPC-via-`wylde-memgraph` route.
            let client = crate::memory::memgraph::current_traversal_impl();
            crate::memory::memgraph::actions::run_graph_query(args, &client).await
        },
    ));
}

async fn run_tool_search(args: Value) -> Result<Value, IpcError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if query.is_empty() {
        return Ok(json!({"status": "error", "error": "'query' is required"}));
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(5);
    let group_filter = args
        .get("group")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty());

    // Query the global registry. This means the tool can discover every
    // registered tool — including deferred stubs, which is intentional:
    // the model needs to know what's coming as well as what's available.
    let registry = crate::tooling::registry::global();
    let entries = registry.canonical_entries();

    let mut candidates: Vec<Value> = Vec::new();
    for entry in &entries {
        if let Some(g) = &group_filter {
            if &entry.group != g {
                continue;
            }
        }
        let score = score_match(
            &entry.id,
            &entry.name,
            &entry.description,
            &entry.group,
            &query,
        );
        if score <= 0.0 {
            continue;
        }
        let status = match &entry.kind {
            crate::tooling::registry::HandlerKind::Active(_) => "active",
            crate::tooling::registry::HandlerKind::Deferred { .. } => "deferred",
        };
        candidates.push(json!({
            "tool_id": entry.id,
            "name": entry.name,
            "group": entry.group,
            "score": score,
            "description": entry.description.chars().take(300).collect::<String>(),
            "status": status,
        }));
    }

    candidates.sort_by(|a, b| {
        let bs = b["score"].as_f64().unwrap_or(0.0);
        let as_ = a["score"].as_f64().unwrap_or(0.0);
        bs.partial_cmp(&as_).unwrap_or(std::cmp::Ordering::Equal)
    });
    let truncated: Vec<Value> = candidates.into_iter().take(limit).collect();

    Ok(json!({
        "status": "success",
        "query": query,
        "results": truncated.clone(),
        "count": truncated.len(),
        "scanned": entries.len(),
    }))
}

fn score_match(id: &str, name: &str, description: &str, group: &str, query: &str) -> f64 {
    if query.is_empty() {
        return 0.0;
    }
    let q_tokens: HashSet<String> = TOKEN_RE
        .find_iter(query)
        .map(|m| m.as_str().to_lowercase())
        .filter(|t| !STOPWORDS.contains(t.as_str()))
        .collect();
    if q_tokens.is_empty() {
        return 0.0;
    }
    let blob = format!("{id} {name} {description} {group}").to_lowercase();
    let name_tokens: HashSet<String> = TOKEN_RE
        .find_iter(&blob)
        .map(|m| m.as_str().to_lowercase())
        .collect();
    let overlap: HashSet<&String> = q_tokens.intersection(&name_tokens).collect();
    let id_lc = id.to_lowercase();
    if overlap.is_empty() {
        let sub = q_tokens
            .iter()
            .filter(|q| blob.contains(q.as_str()))
            .count();
        return round3(0.3 * sub as f64 / q_tokens.len().max(1) as f64);
    }
    let mut score = overlap.len() as f64 / q_tokens.len().max(1) as f64;
    if q_tokens.iter().any(|q| id_lc.contains(q)) {
        score += 0.25;
    }
    round3(score.min(1.0))
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_match_returns_zero_for_no_query() {
        let s = score_match("x", "x", "", "", "");
        assert_eq!(s, 0.0);
    }

    #[test]
    fn score_match_overlap_increases_score() {
        let s = score_match(
            "memory_search",
            "memory.search",
            "search the memory layer",
            "memory",
            "search memory",
        );
        assert!(s > 0.5);
    }

    #[test]
    fn score_match_id_substring_boost() {
        // Token "bar" is a query token; the id "foo_bar_baz" contains
        // it as a substring but the tokeniser sees "foo_bar_baz" as one
        // token (underscores are word chars). Falls through to the
        // substring branch which scores 0.3 per matched token.
        let s = score_match("foo_bar_baz", "x.x", "", "", "bar");
        assert!(s > 0.0, "should score > 0; got {s}");
        assert!(
            s <= 0.3 + f64::EPSILON,
            "substring branch caps at 0.3; got {s}"
        );

        // When a clean token overlap exists AND the id contains the
        // query token as substring, the +0.25 boost fires.
        let s2 = score_match("read_file", "fs.read_file", "read a file", "fs", "read");
        assert!(
            s2 >= 1.0 - f64::EPSILON,
            "expected >= 1.0 with overlap + id substring; got {s2}"
        );
    }

    #[tokio::test]
    async fn tool_search_errors_on_missing_query() {
        let v = run_tool_search(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn tool_search_returns_canonical_entries_from_global() {
        // Force the global registry to materialise.
        let _ = crate::tooling::registry::global();
        let v = run_tool_search(json!({"query": "read file"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        let results = v["results"].as_array().unwrap();
        // read_file should be in the top-3 results.
        let found = results
            .iter()
            .any(|r| r["tool_id"] == "read_file" || r["tool_id"] == "list_files");
        assert!(found, "expected fs tools in results: {results:?}");
    }
}
