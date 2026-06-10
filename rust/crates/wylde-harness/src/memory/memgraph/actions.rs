//! `meta.graph_query` tool handler.
//!
//! Rust port of `Core/harness/tooling/tools/meta/graph_query/graph_query.py`.
//! Three paths, picked by what the caller supplies:
//!
//! * **Hybrid path** (Phase 7.B-3+) — when the caller supplies a
//!   precomputed `query_vector` (the Rust embedder isn't wired yet),
//!   run `rag::search` for vector seeds → `expand_by_graph` for graph
//!   neighbours → `merge_and_rank` to fuse them. Returns the same
//!   envelope shape Python's hybrid path returns (`vector_seeds`
//!   populated, `source: "memgraph+vector"`).
//! * **Entity path** — when the caller supplies an explicit `entities`
//!   list, drive `client::traverse` directly.
//! * **`q` path** — extract candidate identifiers from the query
//!   (lower-bound keyword fallback) and call `traverse` with them.
//!
//! Failure model matches Python: empty `results` + `count: 0` for every
//! error path. Never raises — the tool is meant to fail soft so a
//! planner doesn't abort over a missing graph backend.
//!
//! Seam iii filler — wired into `tooling::tools::meta::register` so the
//! catalog entry flips from deferred to active.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::memory::common::{data_dir, embed_dim};
use crate::memory::memgraph::graph_retrieval::{expand_by_graph, ExpandOptions};
use crate::memory::memgraph::transport::MemgraphTraversal;
use crate::memory::rag::merge::merge_and_rank;
use crate::memory::rag::search::{search, Hit, SearchError};
use crate::memory::rag::store::TieredStore;
use crate::memory::rag::tiers::is_known_tier;

/// Token regex matching identifier-shaped substrings. Mirrors Python's
/// `_QUERY_IDENT_RE` — `\b([A-Za-z_][A-Za-z0-9_]{2,})\b`.
static IDENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]{2,})\b").expect("static ident regex"));

/// Stopwords mirroring Python's `_STOP` set so identifier extraction
/// returns the same shortlist for identical queries.
static STOPWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let mut s = HashSet::new();
    for w in [
        "the", "what", "how", "why", "when", "where", "does", "find", "show", "tell", "about",
        "with", "for", "and", "that", "this", "from", "into", "are", "you", "can", "use", "uses",
        "using",
    ] {
        s.insert(w);
    }
    s
});

fn extract_entities(query: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for m in IDENT_RE.find_iter(query) {
        let tok = m.as_str().to_owned();
        let low = tok.to_lowercase();
        if STOPWORDS.contains(low.as_str()) || seen.contains(&low) {
            continue;
        }
        seen.insert(low);
        out.push(tok);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Coerce a `traverse` reply's `data` field into a `chunks` list, the
/// same way `_normalize_chunks` in Python does.
fn normalize_chunks(data: &Value) -> Vec<Value> {
    if let Value::Object(map) = data {
        if let Some(Value::Array(chunks)) = map.get("chunks") {
            return chunks.clone();
        }
        if let Some(Value::Object(nested)) = map.get("data") {
            if let Some(Value::Array(chunks)) = nested.get("chunks") {
                return chunks.clone();
            }
        }
        return Vec::new();
    }
    if let Value::Array(list) = data {
        return list.clone();
    }
    Vec::new()
}

/// Tool handler.
pub async fn run_graph_query<T: MemgraphTraversal>(
    args: Value,
    client: &T,
) -> Result<Value, IpcError> {
    let query = args
        .get("q")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let explicit_entities: Vec<String> = args
        .get("entities")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let workspace_id = args
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    let max_hops = clamp_int(args.get("max_hops"), 1, 1, 4);
    let limit = clamp_int(args.get("limit"), 10, 1, 50);
    let vector_k = clamp_int(args.get("vector_k"), 5, 1, 20);
    let tier = args
        .get("tier")
        .or_else(|| args.get("memory_type"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(t) = &tier {
        if !is_known_tier(t) {
            return Ok(empty_envelope(&explicit_entities, &query, "unknown tier"));
        }
    }

    let query_vector = parse_float_array(args.get("query_vector"));

    let entity_seeds = if !explicit_entities.is_empty() {
        explicit_entities.clone()
    } else if !query.is_empty() {
        extract_entities(&query, 12)
    } else if query_vector.is_some() {
        Vec::new()
    } else {
        return Ok(json!({"error": "either 'q', 'entities', or 'query_vector' is required"}));
    };

    // ── Hybrid path ──────────────────────────────────────────────────
    if let Some(qv) = query_vector {
        return run_hybrid_path(
            client,
            &query,
            &entity_seeds,
            qv,
            tier.as_deref(),
            &workspace_id,
            vector_k as usize,
            max_hops,
            limit as usize,
        )
        .await;
    }

    // ── Entity-only path ─────────────────────────────────────────────
    if entity_seeds.is_empty() {
        return Ok(empty_envelope(
            &explicit_entities,
            &query,
            "no entities extracted from query",
        ));
    }

    let req = crate::memory::memgraph::client::TraverseRequest {
        entities: entity_seeds.clone(),
        max_hops,
        limit,
        workspace: if workspace_id.is_empty() {
            None
        } else {
            Some(workspace_id.clone())
        },
        decay_alpha: None,
        rel_depths: None,
    };
    let reply = client.traverse(req).await;
    if !reply.ok {
        return Ok(empty_envelope(
            &entity_seeds,
            &query,
            "graph backend unavailable",
        ));
    }

    let chunks = normalize_chunks(&reply.data);
    let count = chunks.len();

    let mut envelope = json!({
        "entities": entity_seeds,
        "results": chunks,
        "count": count,
        "source": "memgraph",
        "vector_seeds": [],
    });
    if !query.is_empty() {
        envelope["q"] = json!(query);
    }
    Ok(envelope)
}

#[allow(clippy::too_many_arguments)]
async fn run_hybrid_path<T: MemgraphTraversal>(
    client: &T,
    query: &str,
    entity_seeds: &[String],
    query_vector: Vec<f32>,
    tier: Option<&str>,
    workspace_id: &str,
    vector_k: usize,
    hops: u32,
    limit: usize,
) -> Result<Value, IpcError> {
    let store = TieredStore::open_at(&data_dir(), embed_dim());
    let vector_hits: Vec<Hit> = match search(&store, query_vector, tier, vector_k) {
        Ok(hits) => hits,
        Err(SearchError::UnknownTier(_)) => {
            return Ok(empty_envelope(entity_seeds, query, "unknown tier"));
        }
        Err(SearchError::Vector(_)) => {
            return Ok(empty_envelope(
                entity_seeds,
                query,
                "vector store unavailable",
            ));
        }
    };
    let candidates: Vec<Value> = vector_hits.iter().map(Hit::to_value).collect();

    let opts = ExpandOptions {
        workspace_id: workspace_id.to_owned(),
        hops,
        max_extra: limit as u32,
        seed_entities: entity_seeds.to_vec(),
    };
    let graph_hits = expand_by_graph(client, candidates.clone(), opts).await;

    let ranked = merge_and_rank(&vector_hits, &graph_hits, limit);
    let vector_seeds: Vec<Value> = vector_hits.iter().map(Hit::to_value).collect();
    let count = ranked.len();
    let source = if graph_hits.is_empty() {
        "vector"
    } else if vector_hits.is_empty() {
        "memgraph"
    } else {
        "memgraph+vector"
    };

    let mut envelope = json!({
        "entities": entity_seeds,
        "results": ranked,
        "count": count,
        "source": source,
        "vector_seeds": vector_seeds,
    });
    if !query.is_empty() {
        envelope["q"] = json!(query);
    }
    Ok(envelope)
}

fn parse_float_array(v: Option<&Value>) -> Option<Vec<f32>> {
    let arr = v?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let out: Option<Vec<f32>> = arr.iter().map(|x| x.as_f64().map(|n| n as f32)).collect();
    out.filter(|v| !v.is_empty())
}

fn clamp_int(v: Option<&Value>, fallback: u32, lo: u32, hi: u32) -> u32 {
    let raw = v.and_then(Value::as_i64).unwrap_or(fallback as i64);
    raw.max(lo as i64).min(hi as i64) as u32
}

fn empty_envelope(entities: &[String], query: &str, err: &str) -> Value {
    let mut v = json!({
        "entities": entities,
        "results": [],
        "count": 0,
        "source": "none",
        "error": err,
        "vector_seeds": [],
    });
    if !query.is_empty() {
        v["q"] = json!(query);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memgraph::client::mock;
    use crate::memory::rag::store::TierRecord;
    use crate::memory::rag::test_support::TestEnv;
    use wylde_shared::ipc::Reply;

    #[test]
    fn extract_entities_filters_stopwords_and_dedupes() {
        let ents = extract_entities(
            "how does the foo_bar configure the baz_qux for the foo_bar",
            12,
        );
        // "how", "does", "the", "for" filtered. "foo_bar" deduped.
        assert_eq!(
            ents,
            vec![
                "foo_bar".to_owned(),
                "configure".to_owned(),
                "baz_qux".to_owned()
            ]
        );
    }

    #[test]
    fn extract_entities_caps_at_limit() {
        let ents = extract_entities("alpha beta gamma delta epsilon zeta eta", 3);
        assert_eq!(ents.len(), 3);
    }

    #[test]
    fn normalize_chunks_accepts_chunks_key() {
        let data = json!({"chunks": [{"id": "c1"}, {"id": "c2"}]});
        let out = normalize_chunks(&data);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], "c1");
    }

    #[test]
    fn normalize_chunks_unwraps_nested_data_key() {
        let data = json!({"data": {"chunks": [{"id": "c3"}]}});
        let out = normalize_chunks(&data);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "c3");
    }

    #[test]
    fn normalize_chunks_accepts_bare_list() {
        let data = json!([{"id": "c4"}]);
        let out = normalize_chunks(&data);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn normalize_chunks_returns_empty_on_unknown_shape() {
        let data = json!({"weird": "shape"});
        assert!(normalize_chunks(&data).is_empty());
        assert!(normalize_chunks(&json!("string")).is_empty());
    }

    #[tokio::test]
    async fn run_graph_query_explicit_entities_hits_traverse() {
        let (client, handle) = mock::new_with_static_ok(json!({"chunks": [{"id": "c1"}]}));
        let out = run_graph_query(
            json!({"entities": ["foo", "bar"], "max_hops": 2, "limit": 5}),
            &client,
        )
        .await
        .unwrap();
        let calls = handle.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "/traverse");
        assert_eq!(calls[0].payload["entities"][0], "foo");
        assert_eq!(out["count"], 1);
        assert_eq!(out["source"], "memgraph");
    }

    #[tokio::test]
    async fn run_graph_query_q_extracts_entities_then_traverses() {
        let (client, handle) = mock::new_with_static_ok(json!({"chunks": []}));
        run_graph_query(json!({"q": "find foo_bar in baz"}), &client)
            .await
            .unwrap();
        let calls = handle.calls();
        assert_eq!(calls.len(), 1);
        let entities = calls[0].payload["entities"]
            .as_array()
            .expect("entities array");
        assert!(entities.iter().any(|v| v.as_str() == Some("foo_bar")));
    }

    #[tokio::test]
    async fn run_graph_query_returns_error_when_neither_q_nor_entities() {
        let (client, _) = mock::new_with_static_ok(Value::Null);
        let out = run_graph_query(json!({}), &client).await.unwrap();
        // Phase 7.B-3 expanded the trigger set: the hybrid path also
        // accepts a raw query_vector, so the error message names all three.
        assert_eq!(
            out["error"],
            "either 'q', 'entities', or 'query_vector' is required"
        );
    }

    #[tokio::test]
    async fn run_graph_query_fail_soft_on_backend_unavailable() {
        let (client, _) =
            mock::new_with_responder(|_| Reply::err_msg("pipe_connect", "no service"));
        let out = run_graph_query(json!({"entities": ["foo"]}), &client)
            .await
            .unwrap();
        assert_eq!(out["count"], 0);
        assert_eq!(out["source"], "none");
        assert_eq!(out["error"], "graph backend unavailable");
    }

    #[tokio::test]
    async fn run_graph_query_q_only_with_no_extractable_entities_returns_empty() {
        let (client, handle) = mock::new_with_static_ok(json!({"chunks": []}));
        let out = run_graph_query(json!({"q": "is it ok"}), &client)
            .await
            .unwrap();
        // No identifier-shaped tokens after filtering stopwords.
        assert_eq!(handle.calls().len(), 0, "must not hit the backend");
        assert_eq!(out["count"], 0);
        assert!(out["error"].is_string());
    }

    #[tokio::test]
    async fn run_graph_query_clamps_out_of_range_params() {
        let (client, handle) = mock::new_with_static_ok(json!({"chunks": []}));
        run_graph_query(
            json!({"entities": ["x"], "max_hops": 99, "limit": 9999}),
            &client,
        )
        .await
        .unwrap();
        let payload = &handle.calls()[0].payload;
        assert_eq!(payload["max_hops"], 4); // clamped 1..=4
        assert_eq!(payload["limit"], 50); // clamped 1..=50
    }

    #[tokio::test]
    async fn run_graph_query_passes_workspace_id_through() {
        let (client, handle) = mock::new_with_static_ok(json!({"chunks": []}));
        run_graph_query(json!({"entities": ["x"], "workspace_id": "ws-42"}), &client)
            .await
            .unwrap();
        let payload = &handle.calls()[0].payload;
        assert_eq!(payload["workspace"], "ws-42");
    }

    // ── hybrid path ─────────────────────────────────────────────────────

    fn seed_hybrid_store() {
        std::env::set_var("WYLDE_EMBED_DIM", "4");
        let dir = std::env::var_os("WYLDE_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap();
        let mut s = TieredStore::open_at(&dir, 4);
        s.insert(
            TierRecord::new("v-a", "alpha", "episodic", 0.4, "", ""),
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        s.insert(
            TierRecord::new("v-b", "beta", "core", 0.9, "", ""),
            Some(vec![0.0, 1.0, 0.0, 0.0]),
        )
        .unwrap();
        s.save().unwrap();
    }

    #[tokio::test]
    async fn hybrid_path_merges_vector_and_graph_hits() {
        let _env = TestEnv::new();
        seed_hybrid_store();
        // Memgraph returns one graph-only neighbour anchored on the
        // entity name we pass as a seed.
        let (client, _) = mock::new_with_static_ok(json!({
            "chunks": [{"id": "g-1", "hops": 1, "via_entities": ["the Wylde user"]}]
        }));
        let out = run_graph_query(
            json!({
                "q": "tell me about the Wylde user",
                "query_vector": [1.0, 0.0, 0.0, 0.0],
                "vector_k": 3,
                "limit": 5,
            }),
            &client,
        )
        .await
        .unwrap();
        assert_eq!(out["source"], "memgraph+vector");
        let results = out["results"].as_array().unwrap();
        let ids: Vec<&str> = results.iter().filter_map(|r| r["id"].as_str()).collect();
        assert!(ids.contains(&"g-1"), "graph hit present: {ids:?}");
        assert!(ids.contains(&"v-a"), "vector hit present: {ids:?}");
        // vector_seeds populated.
        let seeds = out["vector_seeds"].as_array().unwrap();
        assert!(!seeds.is_empty(), "vector_seeds populated");
    }

    #[tokio::test]
    async fn hybrid_path_falls_back_to_vector_when_graph_empty() {
        let _env = TestEnv::new();
        seed_hybrid_store();
        let (client, _) = mock::new_with_static_ok(json!({"chunks": []}));
        let out = run_graph_query(
            json!({
                "q": "the Wylde user",
                "query_vector": [1.0, 0.0, 0.0, 0.0],
                "vector_k": 3,
                "limit": 5,
            }),
            &client,
        )
        .await
        .unwrap();
        assert_eq!(out["source"], "vector");
        let count = out["count"].as_i64().unwrap();
        assert!(count > 0, "vector hits survive when graph empty");
    }

    #[tokio::test]
    async fn hybrid_path_unknown_tier_returns_empty_envelope() {
        let _env = TestEnv::new();
        seed_hybrid_store();
        let (client, _) = mock::new_with_static_ok(json!({"chunks": []}));
        let out = run_graph_query(
            json!({
                "q": "anything",
                "query_vector": [1.0, 0.0, 0.0, 0.0],
                "tier": "junk",
            }),
            &client,
        )
        .await
        .unwrap();
        assert_eq!(out["count"], 0);
        assert_eq!(out["error"], "unknown tier");
    }

    #[tokio::test]
    async fn hybrid_path_works_with_query_vector_only_no_query_text() {
        let _env = TestEnv::new();
        seed_hybrid_store();
        let (client, _) = mock::new_with_static_ok(json!({"chunks": []}));
        let out = run_graph_query(
            json!({
                "query_vector": [1.0, 0.0, 0.0, 0.0],
                "limit": 5,
            }),
            &client,
        )
        .await
        .unwrap();
        // No q, no entities → entity_seeds is empty; vector path still works.
        assert_eq!(out["source"], "vector");
    }
}
