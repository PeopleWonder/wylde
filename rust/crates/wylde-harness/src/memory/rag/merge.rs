//! `_merge_and_rank` — combined vector + graph hit fusion.
//!
//! Direct Rust port of
//! `Core/harness/tooling/tools/meta/graph_query/graph_query.py::
//! _merge_and_rank`. Keeps the formula identical:
//!
//! * Pure vector hit: `combined = ALPHA * similarity`, source="vector".
//! * Pure graph hit:  `combined = (1 - ALPHA) * graph_similarity`, source="graph".
//! * Both retrievers agree (same `id`): take the better of the two
//!   weighted scores, add a +0.05 agreement bonus, source="vector+graph".
//!
//! Sort descending by `combined_score`, trim to `limit`. The bonus is
//! deliberately small — it nudges agreement-hits above near-tied pure
//! hits without overwhelming a single strong retriever.

use serde_json::{json, Value};

use crate::memory::memgraph::GraphHit;
use crate::memory::rag::search::Hit;

/// Combined-score weight between vector and graph signals. Pinned to
/// the Python constant so the parity test can compare results
/// numerically. Higher α puts more weight on the vector signal.
pub const COMBINED_ALPHA: f64 = 0.6;

/// Agreement bonus added when a chunk surfaces in both the vector and
/// graph stages. Same value as Python (`+0.05`).
pub const AGREEMENT_BONUS: f64 = 0.05;

/// Combine vector + graph hits into one ranked list. The output rows
/// are JSON values matching the Python tool envelope's shape — the
/// caller can drop them straight into the `results` field.
pub fn merge_and_rank(vector_hits: &[Hit], graph_hits: &[GraphHit], limit: usize) -> Vec<Value> {
    use std::collections::HashMap;

    // Preserve first-seen ordering by using a sidecar Vec; the map
    // keeps lookups O(1) and the Vec keeps later sort deterministic.
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Value> = HashMap::new();

    for hit in vector_hits {
        if hit.id.is_empty() {
            continue;
        }
        let sim = f64::from(hit.similarity);
        let entry = json!({
            "id": hit.id,
            "content": hit.content,
            "memory_type": hit.memory_type,
            "similarity": sim,
            "score": hit.score,
            "created_at": hit.created_at,
            "session_id": hit.session_id,
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
            let weighted_pair =
                COMBINED_ALPHA * existing_vec + (1.0 - COMBINED_ALPHA) * graph_sim;
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

    fn hit(id: &str, sim: f32) -> Hit {
        Hit {
            id: id.into(),
            content: format!("content-{id}"),
            memory_type: "episodic".into(),
            similarity: sim,
            score: 0.5,
            created_at: 0.0,
            session_id: String::new(),
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
        let v = [hit("a", 0.8)];
        let ranked = merge_and_rank(&v, &[], 10);
        assert_eq!(ranked.len(), 1);
        let combined = ranked[0]["combined_score"].as_f64().unwrap();
        // 0.6 * 0.8 = 0.48 (epsilon = 1e-6: f32-precision 0.8 widens to
        // ~0.4800000071 after f32→f64, which is well below a meaningful
        // ranking delta).
        assert!((combined - 0.48).abs() < 1e-6, "got {combined}");
        assert_eq!(ranked[0]["source"], "vector");
    }

    #[test]
    fn pure_graph_hit_uses_inverse_alpha_weighted_similarity() {
        let g = [ghit("b", 1, 0.5)];
        let ranked = merge_and_rank(&[], &g, 10);
        assert_eq!(ranked.len(), 1);
        let combined = ranked[0]["combined_score"].as_f64().unwrap();
        // 0.4 * 0.5 = 0.20. Graph similarity is f64 so 1e-9 holds here.
        assert!((combined - 0.20).abs() < 1e-9, "got {combined}");
        assert_eq!(ranked[0]["source"], "graph");
    }

    #[test]
    fn both_retrievers_agree_adds_bonus() {
        let v = [hit("c", 0.8)];
        let g = [ghit("c", 1, 0.5)];
        let ranked = merge_and_rank(&v, &g, 10);
        assert_eq!(ranked.len(), 1);
        let combined = ranked[0]["combined_score"].as_f64().unwrap();
        // max(0.6 * 0.8, 0.6 * 0.8 + 0.4 * 0.5) + 0.05
        // = max(0.48, 0.68) + 0.05 = 0.73. Vector similarity is f32 here,
        // so the same 1e-6 epsilon as the pure-vector test applies.
        assert!((combined - 0.73).abs() < 1e-6, "got {combined}");
        assert_eq!(ranked[0]["source"], "vector+graph");
        assert_eq!(ranked[0]["graph_hops"], 1);
    }

    #[test]
    fn ranking_is_descending_by_combined_score() {
        let v = [hit("low", 0.1), hit("high", 0.9), hit("mid", 0.5)];
        let ranked = merge_and_rank(&v, &[], 10);
        let ids: Vec<&str> = ranked
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["high", "mid", "low"]);
    }

    #[test]
    fn limit_truncates_lower_scored_rows() {
        let v: Vec<Hit> = (0..10)
            .map(|i| hit(&format!("v{i}"), (10 - i) as f32 / 10.0))
            .collect();
        let ranked = merge_and_rank(&v, &[], 3);
        assert_eq!(ranked.len(), 3);
        // Top 3 are v0, v1, v2 (similarities 1.0, 0.9, 0.8).
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
        let v = [hit("", 1.0), hit("real", 0.5)];
        let ranked = merge_and_rank(&v, &[], 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0]["id"], "real");
    }
}
