//! Vector + graph hit fusion for the `meta.graph_query` hybrid path.
//!
//! Relocated from the retired `memory/rag/merge.rs` (memory plan **M7** —
//! rag retirement). The fusion is retrieval logic, not RAG: it belongs
//! beside the graph-expansion code (`graph_retrieval`) that produces the
//! other half of the input. The formula is unchanged from the rag port —
//! itself a direct port of
//! `Core/harness/tooling/tools/meta/graph_query/graph_query.py::
//! _merge_and_rank`:
//!
//! * Pure vector hit: `combined = ALPHA * similarity`, source="vector".
//! * Pure graph hit:  `combined = (1 - ALPHA) * graph_similarity`, source="graph".
//! * Both retrievers agree (same `id`): take the better of the two
//!   weighted scores, add a +0.05 agreement bonus, source="vector+graph".
//!
//! Sort descending by `combined_score`, trim to `limit`.
//!
//! ## What changed at M7
//!
//! The vector seeds previously came from the deleted tiered RAG store
//! (`rag::search::Hit`). They now come from the **long-term memory
//! store** (`long_term::SearchHit`), the one vector store with live,
//! embed-on-write ingest. [`VectorSeed`] is the small projection
//! `merge_and_rank` needs, built via [`VectorSeed::from_long_term`]. The
//! emitted row shape is byte-for-byte the same as the rag-era fusion
//! (`memory_type`/`session_id` kept for envelope stability — the
//! long-term tier has no per-session attribution, so `session_id` is
//! empty and `memory_type` is the constant `"long_term"`).

use serde_json::{json, Value};

use crate::memory::long_term::SearchHit;
use crate::memory::memgraph::graph_retrieval::GraphHit;

/// Combined-score weight between vector and graph signals. Pinned to
/// the Python constant so the parity test can compare results
/// numerically. Higher α puts more weight on the vector signal.
pub const COMBINED_ALPHA: f64 = 0.6;

/// Agreement bonus added when a chunk surfaces in both the vector and
/// graph stages. Same value as Python (`+0.05`).
pub const AGREEMENT_BONUS: f64 = 0.05;

/// Memory-type tag stamped on every vector seed. The long-term store is
/// a single tier (unlike the retired four-tier RAG store), so this is a
/// constant rather than a per-record field.
const LONG_TERM_MEMORY_TYPE: &str = "long_term";

/// One vector-stage seed for fusion. The minimal projection of a
/// long-term [`SearchHit`] that [`merge_and_rank`] needs to build its
/// output row.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorSeed {
    pub id: String,
    pub content: String,
    pub similarity: f64,
    pub score: f64,
    pub created_at: f64,
    pub source_path: String,
}

impl VectorSeed {
    /// Project a long-term search hit into a fusion seed. `body` becomes
    /// `content` and the record's `source` becomes `source_path` (the
    /// long-term store's notion of provenance).
    pub fn from_long_term(hit: &SearchHit) -> Self {
        Self {
            id: hit.id.clone(),
            content: hit.body.clone(),
            similarity: hit.similarity,
            score: hit.score,
            created_at: hit.created_at,
            source_path: hit.source.clone(),
        }
    }

    /// The envelope row a bare vector seed renders to — matches the
    /// `vector_seeds` shape the tool has always returned.
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "content": self.content,
            "memory_type": LONG_TERM_MEMORY_TYPE,
            "similarity": self.similarity,
            "score": self.score,
            "created_at": self.created_at,
            "session_id": "",
            "source_path": self.source_path,
        })
    }
}

/// Combine vector + graph hits into one ranked list. The output rows
/// are JSON values matching the tool envelope's shape — the caller can
/// drop them straight into the `results` field.
pub fn merge_and_rank(
    vector_hits: &[VectorSeed],
    graph_hits: &[GraphHit],
    limit: usize,
) -> Vec<Value> {
    use std::collections::HashMap;

    // Preserve first-seen ordering by using a sidecar Vec; the map
    // keeps lookups O(1) and the Vec keeps later sort deterministic.
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Value> = HashMap::new();

    for hit in vector_hits {
        if hit.id.is_empty() {
            continue;
        }
        let sim = hit.similarity;
        let entry = json!({
            "id": hit.id,
            "content": hit.content,
            "memory_type": LONG_TERM_MEMORY_TYPE,
            "similarity": sim,
            "score": hit.score,
            "created_at": hit.created_at,
            "session_id": "",
            "source_path": hit.source_path,
            "vector_similarity": sim,
            "graph_hops": Value::Null,
            "graph_similarity": 0.0,
            "combined_score": COMBINED_ALPHA * sim,
            "source": "vector",
        });
        order.push(hit.id.clone());
        by_id.insert(hit.id.clone(), entry);
    }

    for hit in graph_hits {
        if hit.id.is_empty() {
            continue;
        }
        let graph_sim = hit.similarity;
        let hops = hit.hops;
        if let Some(existing) = by_id.get_mut(&hit.id) {
            // Both retrievers agree — take the better of the weighted
            // averages, add the agreement bonus.
            let existing_combined = existing
                .get("combined_score")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let existing_vec = existing
                .get("vector_similarity")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let weighted_pair = COMBINED_ALPHA * existing_vec + (1.0 - COMBINED_ALPHA) * graph_sim;
            let combined = existing_combined.max(weighted_pair) + AGREEMENT_BONUS;
            existing["graph_hops"] = json!(hops);
            existing["graph_similarity"] = json!(graph_sim);
            existing["combined_score"] = json!(combined);
            existing["source"] = json!("vector+graph");
            // Hydrate path / via_entities if not already present.
            if existing.get("path").map(Value::is_null).unwrap_or(true) {
                existing["path"] = json!(hit.path);
            }
            if existing.get("via_entities").is_none() {
                existing["via_entities"] = json!(hit.via_entities);
            }
        } else {
            let entry = json!({
                "id": hit.id,
                "content": hit.content,
                "path": hit.path,
                "via_entities": hit.via_entities,
                "vector_similarity": 0.0,
                "graph_hops": hops,
                "graph_similarity": graph_sim,
                "combined_score": (1.0 - COMBINED_ALPHA) * graph_sim,
                "source": "graph",
            });
            order.push(hit.id.clone());
            by_id.insert(hit.id.clone(), entry);
        }
    }

    let mut ranked: Vec<Value> = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();
    ranked.sort_by(|a, b| {
        let bs = b
            .get("combined_score")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let as_ = a
            .get("combined_score")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        bs.partial_cmp(&as_).unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.truncate(limit);
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(id: &str, sim: f64) -> VectorSeed {
        VectorSeed {
            id: id.into(),
            content: format!("content-{id}"),
            similarity: sim,
            score: 0.5,
            created_at: 0.0,
            source_path: String::new(),
        }
    }

    fn ghit(id: &str, hops: u32, sim: f64) -> GraphHit {
        GraphHit {
            id: id.into(),
            path: format!("path-{id}.py"),
            content: format!("graph-content-{id}"),
            hops,
            via_entities: vec!["e1".into()],
            similarity: sim,
        }
    }

    #[test]
    fn pure_vector_hit_uses_alpha_weighted_similarity() {
        let v = [seed("a", 0.8)];
        let ranked = merge_and_rank(&v, &[], 10);
        assert_eq!(ranked.len(), 1);
        let combined = ranked[0]["combined_score"].as_f64().unwrap();
        // 0.6 * 0.8 = 0.48.
        assert!((combined - 0.48).abs() < 1e-9, "got {combined}");
        assert_eq!(ranked[0]["source"], "vector");
        assert_eq!(ranked[0]["memory_type"], "long_term");
    }

    #[test]
    fn pure_graph_hit_uses_inverse_alpha_weighted_similarity() {
        let g = [ghit("b", 1, 0.5)];
        let ranked = merge_and_rank(&[], &g, 10);
        assert_eq!(ranked.len(), 1);
        let combined = ranked[0]["combined_score"].as_f64().unwrap();
        // 0.4 * 0.5 = 0.20.
        assert!((combined - 0.20).abs() < 1e-9, "got {combined}");
        assert_eq!(ranked[0]["source"], "graph");
    }

    #[test]
    fn both_retrievers_agree_adds_bonus() {
        let v = [seed("c", 0.8)];
        let g = [ghit("c", 1, 0.5)];
        let ranked = merge_and_rank(&v, &g, 10);
        assert_eq!(ranked.len(), 1);
        let combined = ranked[0]["combined_score"].as_f64().unwrap();
        // max(0.6 * 0.8, 0.6 * 0.8 + 0.4 * 0.5) + 0.05
        // = max(0.48, 0.68) + 0.05 = 0.73.
        assert!((combined - 0.73).abs() < 1e-9, "got {combined}");
        assert_eq!(ranked[0]["source"], "vector+graph");
        assert_eq!(ranked[0]["graph_hops"], 1);
    }

    #[test]
    fn ranking_is_descending_by_combined_score() {
        let v = [seed("low", 0.1), seed("high", 0.9), seed("mid", 0.5)];
        let ranked = merge_and_rank(&v, &[], 10);
        let ids: Vec<&str> = ranked.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(ids, vec!["high", "mid", "low"]);
    }

    #[test]
    fn limit_truncates_lower_scored_rows() {
        let v: Vec<VectorSeed> = (0..10)
            .map(|i| seed(&format!("v{i}"), (10 - i) as f64 / 10.0))
            .collect();
        let ranked = merge_and_rank(&v, &[], 3);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0]["id"], "v0");
        assert_eq!(ranked[2]["id"], "v2");
    }

    #[test]
    fn empty_inputs_yield_empty_result() {
        assert!(merge_and_rank(&[], &[], 10).is_empty());
    }

    #[test]
    fn graph_only_hit_hydrates_path_and_via_entities() {
        let g = [ghit("g1", 2, 0.4)];
        let ranked = merge_and_rank(&[], &g, 10);
        assert_eq!(ranked[0]["path"], "path-g1.py");
        let entities = ranked[0]["via_entities"].as_array().unwrap();
        assert_eq!(entities[0], "e1");
    }

    #[test]
    fn hits_with_empty_ids_are_skipped() {
        let v = [seed("", 1.0), seed("real", 0.5)];
        let ranked = merge_and_rank(&v, &[], 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0]["id"], "real");
    }

    #[test]
    fn vector_seed_projects_long_term_hit() {
        let hit = SearchHit {
            id: "lt-1".into(),
            body: "the body text".into(),
            source: "reflection".into(),
            importance: 7,
            created_at: 12.0,
            last_used_at: 34.0,
            similarity: 0.9,
            score: 0.81,
        };
        let seed = VectorSeed::from_long_term(&hit);
        assert_eq!(seed.id, "lt-1");
        assert_eq!(seed.content, "the body text");
        assert_eq!(seed.source_path, "reflection");
        assert!((seed.similarity - 0.9).abs() < 1e-9);
        let v = seed.to_value();
        assert_eq!(v["memory_type"], "long_term");
        assert_eq!(v["content"], "the body text");
    }
}
