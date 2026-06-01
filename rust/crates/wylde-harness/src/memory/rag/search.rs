//! Search path — embed query → tier-filtered vector top-K → optional
//! graph expansion via memgraph → merge_and_rank.
//!
//! Rust port of `Core/harness/memory/rag.py::search` plus the graph
//! expansion that `tools/meta/graph_query.py::run_graph_query` glues on
//! top.
//!
//! ## What this slice DOES NOT do
//!
//! The Python `rag_pipeline.ask` runs an LLM-driven pipeline (query
//! decomposition, HyDE expansion, multi-hop follow-ups, cross-encoder
//! rerank, semantic result cache) on top of [`search_with_graph`]. None
//! of that lands in Phase 7.B subtask 3: the LLM-helper functions need a
//! Rust port of the wylde-ollama chat client + the cross-encoder model
//! load, which is a future slice (likely 7.B+ or its own Phase 7.C).
//! The strangler-fig default stays `python` so production traffic still
//! flows through `Core/harness/memory/rag_pipeline.py` until that lands.
//!
//! The tool surface (`rag.ask`) is wired to the Rust handler in this
//! slice with a documented "simplified" implementation: it embeds the
//! query (via the registered embed function — typically the wylde-
//! ollama client), runs [`search_with_graph`], and returns the ranked
//! pool. No HyDE, no decompose, no multi-hop, no cache.

use serde_json::{json, Value};

use crate::memory::memgraph::{expand_by_graph, Client, ExpandOptions, GraphHit};
use crate::memory::rag::merge;
use crate::memory::rag::miss_log;
use crate::memory::rag::store::{TierHit, TieredStore};
use crate::memory::rag::tiers::is_known_tier;

/// Search-result envelope. Each hit carries the underlying record's
/// content plus the cosine similarity the vector store returned. The
/// `to_value` form matches the Python `rag.search` row shape:
/// `{id, content, memory_type, similarity, score, created_at, session_id,
/// source_path}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub similarity: f32,
    pub score: f32,
    pub created_at: f64,
    pub session_id: String,
    pub source_path: String,
}

impl Hit {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "content": self.content,
            "memory_type": self.memory_type,
            "similarity": self.similarity,
            "score": self.score,
            "created_at": self.created_at,
            "session_id": self.session_id,
            "source_path": self.source_path,
        })
    }
}

impl From<TierHit> for Hit {
    fn from(th: TierHit) -> Self {
        Self {
            id: th.record.id,
            content: th.record.content,
            memory_type: th.record.memory_type,
            similarity: th.similarity,
            score: th.record.score,
            created_at: th.record.created_at,
            session_id: th.record.session_id,
            source_path: th.record.source_path,
        }
    }
}

/// Search the tiered store. `tier` is the optional tier filter ("core",
/// "episodic", "semantic", "procedural"); `None` or an unknown string
/// matches Python's "search every tier" sentinel (Python raises on
/// unknown — we follow the same contract via [`SearchError::UnknownTier`]).
///
/// Every call appends a row to `miss_log` so the `rag_misses` /
/// `rag_chunk_usage` tools have a query history to operate on. The log
/// write is best-effort — a disk failure does not break retrieval.
pub fn search(
    store: &TieredStore,
    query_vector: Vec<f32>,
    tier: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>, SearchError> {
    if let Some(t) = tier {
        if !is_known_tier(t) {
            return Err(SearchError::UnknownTier(t.to_owned()));
        }
    }
    let raw = store
        .search_vectors(query_vector, tier, limit)
        .map_err(|e| SearchError::Vector(e.to_string()))?;
    Ok(raw.into_iter().map(Hit::from).collect())
}

/// Search-and-log: identical to [`search`] but also writes a `miss_log`
/// entry. Use this from the tool surface; lower-level callers (e.g.
/// `meta.graph_query` hybrid path) call [`search`] directly so they
/// don't double-log.
pub fn search_logged(
    store: &TieredStore,
    query_text: &str,
    query_vector: Vec<f32>,
    tier: Option<&str>,
    workspace_id: &str,
    limit: usize,
) -> Result<Vec<Hit>, SearchError> {
    let hits = search(store, query_vector, tier, limit)?;
    let values: Vec<Value> = hits.iter().map(Hit::to_value).collect();
    miss_log::log_query(query_text, workspace_id, &values, tier);
    Ok(hits)
}

/// Hybrid retrieval: vector top-K → graph expansion via memgraph →
/// merge_and_rank. Returns the JSON shape `meta.graph_query` wraps in
/// its result envelope.
pub async fn search_with_graph(
    store: &TieredStore,
    client: &Client,
    query_vector: Vec<f32>,
    tier: Option<&str>,
    vector_k: usize,
    graph_opts: ExpandOptions,
) -> Result<HybridResult, SearchError> {
    let vector_hits = search(store, query_vector, tier, vector_k)?;
    let candidate_values: Vec<Value> = vector_hits.iter().map(Hit::to_value).collect();
    let graph_hits: Vec<GraphHit> = expand_by_graph(client, candidate_values.clone(), graph_opts).await;
    let ranked = merge::merge_and_rank(&vector_hits, &graph_hits, vector_k.max(graph_hits.len()).min(50));
    Ok(HybridResult {
        vector_hits,
        graph_hits,
        ranked,
    })
}

/// Composed result of [`search_with_graph`]. Callers (the
/// `meta.graph_query` handler) typically only consume `ranked`; the
/// per-stage lists are kept around so the tool envelope can report
/// `vector_seeds`, just like Python does.
#[derive(Debug)]
pub struct HybridResult {
    pub vector_hits: Vec<Hit>,
    pub graph_hits: Vec<GraphHit>,
    pub ranked: Vec<Value>,
}

/// Search errors.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("unknown tier '{0}'")]
    UnknownTier(String),
    #[error("vector store: {0}")]
    Vector(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memgraph::client::mock;
    use crate::memory::rag::store::{TierRecord, TieredStore};
    use crate::memory::rag::test_support::TestEnv;

    fn seeded_store(td_path: &std::path::Path) -> TieredStore {
        let mut s = TieredStore::open_at(td_path, 4);
        s.insert(
            TierRecord::new(
                "core-a",
                "user identity is the Wylde user",
                "core",
                1.0,
                "",
                "core",
            ),
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        s.insert(
            TierRecord::new(
                "ep-b",
                "ran tests last Tuesday",
                "episodic",
                0.5,
                "",
                "",
            ),
            Some(vec![0.0, 1.0, 0.0, 0.0]),
        )
        .unwrap();
        s.insert(
            TierRecord::new(
                "sem-c",
                "tests live under rust/",
                "semantic",
                0.7,
                "",
                "semantic",
            ),
            Some(vec![0.9, 0.1, 0.0, 0.0]),
        )
        .unwrap();
        s
    }

    #[test]
    fn search_returns_top_k_sorted_by_similarity() {
        let env = TestEnv::new();
        let store = seeded_store(&std::env::var_os("WYLDE_DATA_DIR").map(std::path::PathBuf::from).unwrap());
        let hits = search(&store, vec![1.0, 0.0, 0.0, 0.0], None, 3).unwrap();
        assert_eq!(hits[0].id, "core-a");
        assert_eq!(hits[1].id, "sem-c");
        assert_eq!(hits[2].id, "ep-b");
        drop(env);
    }

    #[test]
    fn search_filters_by_tier() {
        let _env = TestEnv::new();
        let store = seeded_store(&std::env::var_os("WYLDE_DATA_DIR").map(std::path::PathBuf::from).unwrap());
        let hits = search(&store, vec![1.0, 0.0, 0.0, 0.0], Some("semantic"), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sem-c");
    }

    #[test]
    fn search_unknown_tier_returns_error() {
        let _env = TestEnv::new();
        let store = seeded_store(&std::env::var_os("WYLDE_DATA_DIR").map(std::path::PathBuf::from).unwrap());
        let err = search(&store, vec![1.0, 0.0, 0.0, 0.0], Some("garbage"), 5).unwrap_err();
        match err {
            SearchError::UnknownTier(s) => assert_eq!(s, "garbage"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn search_logged_writes_miss_log_row() {
        let _env = TestEnv::new();
        let store = seeded_store(&std::env::var_os("WYLDE_DATA_DIR").map(std::path::PathBuf::from).unwrap());
        // Query a direction that misses every seed (perpendicular).
        let _ = search_logged(
            &store,
            "ortho query",
            vec![0.0, 0.0, 0.0, 1.0],
            None,
            "ws-a",
            5,
        )
        .unwrap();
        let misses = miss_log::list_misses(None, 100);
        // The seeded vectors all have similarity 0 to the query → the
        // query DOES technically return hits (vector store returns
        // top-K regardless of magnitude); the row is logged as a
        // non-miss because hit_count > 0. So we should not see it.
        // Re-query with empty store to actually get a miss row.
        let empty_store = TieredStore::open_at(
            &std::env::var_os("WYLDE_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap()
                .join("empty"),
            4,
        );
        let _ = search_logged(
            &empty_store,
            "really no hits",
            vec![0.0, 0.0, 0.0, 1.0],
            None,
            "ws-a",
            5,
        )
        .unwrap();
        let misses_after = miss_log::list_misses(None, 100);
        assert!(misses_after.len() > misses.len());
        assert!(misses_after
            .iter()
            .any(|r| r["query"] == "really no hits"));
    }

    #[tokio::test]
    async fn search_with_graph_combines_vector_and_graph_hits() {
        let _env = TestEnv::new();
        let store = seeded_store(&std::env::var_os("WYLDE_DATA_DIR").map(std::path::PathBuf::from).unwrap());
        // Mock memgraph returns one graph-only neighbour.
        let (client, _handle) = mock::new_with_static_ok(json!({
            "chunks": [{"id": "graph-1", "hops": 1, "via_entities": ["core-a"]}]
        }));
        // To make multihop call happen, candidates need to have entities. Our seeded
        // store doesn't carry entities — so the candidate path returns empty, and
        // only the entity-seed traverse fires if seed_entities is non-empty.
        let opts = ExpandOptions {
            workspace_id: String::new(),
            hops: 1,
            max_extra: 10,
            seed_entities: vec!["the Wylde user".into()],
        };
        let res = search_with_graph(&store, &client, vec![1.0, 0.0, 0.0, 0.0], None, 3, opts)
            .await
            .unwrap();
        // Vector pass surfaced 3 hits sorted by similarity.
        assert_eq!(res.vector_hits.len(), 3);
        // Graph expansion added one neighbour.
        assert_eq!(res.graph_hits.len(), 1);
        // Ranked union has all four ids (core-a, sem-c, ep-b, graph-1).
        let ids: std::collections::HashSet<String> = res
            .ranked
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert!(ids.contains("graph-1"), "graph hit in merged pool");
        assert!(ids.contains("core-a"));
    }

    #[tokio::test]
    async fn search_with_graph_keeps_vector_when_graph_empty() {
        let _env = TestEnv::new();
        let store = seeded_store(&std::env::var_os("WYLDE_DATA_DIR").map(std::path::PathBuf::from).unwrap());
        let (client, _) = mock::new_with_static_ok(json!({"chunks": []}));
        let res = search_with_graph(
            &store,
            &client,
            vec![1.0, 0.0, 0.0, 0.0],
            None,
            3,
            ExpandOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(res.graph_hits.len(), 0);
        assert_eq!(res.vector_hits.len(), 3);
        // Ranked pool falls back to vector-only.
        assert_eq!(res.ranked.len(), 3);
    }
}
