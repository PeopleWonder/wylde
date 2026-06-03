//! Reader→writer graph feedback for terminal RAG outcomes.
//!
//! Rust port of `Core/harness/memory/rag_feedback.py::record_outcome`.
//! The pipeline calls this at both terminal branches:
//!
//! * `status == "ok"` — strengthen `entity → chunk` edges for every
//!   `(query_entity, cited_chunk)` pair. Each strengthen is a +1.0
//!   weight delta on the `CITED_IN` edge.
//! * `status != "ok"` — record a `weak_retrieval` marker in the miss
//!   log, and draw a low-weight `entity → RetrievalMiss` edge from
//!   every query entity to a sentinel node. Each edge is a +0.25
//!   weight delta on the `RETRIEVAL_MISS` edge.
//!
//! Everything is best-effort. A failed graph write returns `Ok(trace)`
//! with `graph_ok=false`; an unreachable memgraph degrades to the same
//! shape. The Python module's outermost `try/except` belts-and-braces
//! contract is preserved.

use serde_json::{json, Value};

use crate::memory::memgraph::transport::MemgraphTraversal;
use crate::memory::rag::miss_log;

/// Sentinel node every retrieval miss points at. Matches Python's
/// `MISS_SENTINEL` constant exactly.
pub const MISS_SENTINEL: &str = "RetrievalMiss";

const CITED_EDGE: &str = "CITED_IN";
const MISS_EDGE: &str = "RETRIEVAL_MISS";

/// Weight delta for a successful citation. Positive, strong-ish — a
/// single agreement easily overwrites a faint miss.
pub const OK_WEIGHT: f64 = 1.0;

/// Weight delta for a recorded miss. Faint, intentionally small.
pub const MISS_WEIGHT: f64 = 0.25;

/// Small trace summary returned by [`record_outcome`]. Folded into the
/// pipeline's terminal `trace` dict. Never carries an error — failures
/// degrade to `graph_ok=false` so callers can splice the result
/// unconditionally.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeTrace {
    pub graph_edges: u32,
    pub miss_recorded: bool,
    pub graph_ok: bool,
}

impl OutcomeTrace {
    pub fn to_value(&self) -> Value {
        json!({
            "graph_edges": self.graph_edges,
            "miss_recorded": self.miss_recorded,
            "graph_ok": self.graph_ok,
        })
    }
}

/// Feed a terminal RAG outcome back into the knowledge graph.
///
/// `status` is the pipeline's terminal label. Anything other than `"ok"`
/// is treated as a retrieval miss.
pub async fn record_outcome(
    client: &impl MemgraphTraversal,
    query: &str,
    status: &str,
    query_entities: &[String],
    chunk_ids: &[String],
    query_id: &str,
) -> OutcomeTrace {
    let entities: Vec<&str> = query_entities
        .iter()
        .filter_map(|e| {
            let trimmed = e.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .collect();

    if status == "ok" {
        let chunks: Vec<&String> = chunk_ids.iter().filter(|c| !c.is_empty()).collect();
        if entities.is_empty() || chunks.is_empty() {
            return OutcomeTrace {
                graph_edges: 0,
                miss_recorded: false,
                graph_ok: false,
            };
        }
        let mut written = 0u32;
        for ent in &entities {
            for chunk in &chunks {
                let reply = client
                    .upsert_edge(ent, CITED_EDGE, chunk.as_str(), OK_WEIGHT)
                    .await;
                if reply.ok {
                    written += 1;
                }
            }
        }
        return OutcomeTrace {
            graph_edges: written,
            miss_recorded: false,
            graph_ok: written > 0,
        };
    }

    // Non-ok terminal state.
    let trimmed_entities: Vec<String> = entities.iter().map(|s| (*s).to_owned()).collect();
    let miss_recorded = record_weak_marker(query, query_id, &trimmed_entities);
    if trimmed_entities.is_empty() {
        return OutcomeTrace {
            graph_edges: 0,
            miss_recorded,
            graph_ok: false,
        };
    }
    let mut written = 0u32;
    for ent in &trimmed_entities {
        let reply = client
            .upsert_edge(ent, MISS_EDGE, MISS_SENTINEL, MISS_WEIGHT)
            .await;
        if reply.ok {
            written += 1;
        }
    }
    OutcomeTrace {
        graph_edges: written,
        miss_recorded,
        graph_ok: written > 0,
    }
}

fn record_weak_marker(query: &str, query_id: &str, entities: &[String]) -> bool {
    // Mirror Python's truncation at 8 entities to keep the marker row small.
    let capped: Vec<&String> = entities.iter().take(8).collect();
    let ctx = json!({
        "event": "weak_retrieval",
        "query_id": query_id,
        "entities": capped,
    });
    // record_miss returns the id of the row — non-empty means a write
    // landed. Disk failures inside record_miss already swallow errors so
    // we can't distinguish them here; mirror Python's behaviour and
    // treat the write as best-effort.
    let id = miss_log::record_miss(query, Some(ctx));
    !id.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memgraph::client::mock;
    use crate::memory::rag::test_support::TestEnv;
    use wylde_shared::ipc::Reply;

    #[tokio::test]
    async fn ok_outcome_writes_one_edge_per_entity_chunk_pair() {
        let _env = TestEnv::new();
        let (client, handle) = mock::new_with_static_ok(json!({"updated": true}));
        let trace = record_outcome(
            &client,
            "did it work",
            "ok",
            &["foo".into(), "bar".into()],
            &["chunk-1".into(), "chunk-2".into()],
            "q1",
        )
        .await;
        // 2 entities × 2 chunks = 4 edges
        assert_eq!(trace.graph_edges, 4);
        assert!(trace.graph_ok);
        assert!(!trace.miss_recorded);
        let calls = handle.calls();
        assert_eq!(calls.len(), 4);
        for call in &calls {
            assert_eq!(call.method, "/upsert_edge");
            // Client::upsert_edge serialises as {source, label, target,
            // weight_delta} — the test originally checked `edge` which
            // never existed on the wire.
            assert_eq!(call.payload["label"], "CITED_IN");
            assert!(call.payload["weight_delta"].as_f64().unwrap() > 0.99);
        }
    }

    #[tokio::test]
    async fn ok_outcome_with_no_entities_skips_writes() {
        let _env = TestEnv::new();
        let (client, handle) = mock::new_with_static_ok(json!({"updated": true}));
        let trace = record_outcome(
            &client,
            "q",
            "ok",
            &[],
            &["chunk-1".into()],
            "q1",
        )
        .await;
        assert_eq!(trace.graph_edges, 0);
        assert!(!trace.graph_ok);
        assert!(handle.calls().is_empty());
    }

    #[tokio::test]
    async fn ok_outcome_with_no_chunks_skips_writes() {
        let _env = TestEnv::new();
        let (client, handle) = mock::new_with_static_ok(json!({"updated": true}));
        let trace = record_outcome(
            &client,
            "q",
            "ok",
            &["foo".into()],
            &[],
            "q1",
        )
        .await;
        assert_eq!(trace.graph_edges, 0);
        assert!(handle.calls().is_empty());
    }

    #[tokio::test]
    async fn miss_outcome_records_marker_and_low_weight_edges() {
        let _env = TestEnv::new();
        let (client, handle) = mock::new_with_static_ok(json!({"updated": true}));
        let trace = record_outcome(
            &client,
            "did it fail",
            "insufficient_context",
            &["foo".into(), "bar".into()],
            &[],
            "q-miss",
        )
        .await;
        assert!(trace.miss_recorded);
        assert_eq!(trace.graph_edges, 2);
        let calls = handle.calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            // Client::upsert_edge wire shape is {source, label, target,
            // weight_delta} — `edge` / `dst` are stale names from a
            // proposed shape that never landed.
            assert_eq!(call.payload["label"], "RETRIEVAL_MISS");
            assert!((call.payload["weight_delta"].as_f64().unwrap() - 0.25).abs() < 1e-9);
            assert_eq!(call.payload["target"], "RetrievalMiss");
        }
        // The weak_retrieval marker landed in the miss log too.
        let misses = miss_log::list_misses(None, 100);
        let weak = misses
            .iter()
            .find(|m| m["context"]["event"] == "weak_retrieval");
        assert!(weak.is_some());
    }

    #[tokio::test]
    async fn miss_outcome_with_no_entities_only_records_marker() {
        let _env = TestEnv::new();
        let (client, handle) = mock::new_with_static_ok(json!({"updated": true}));
        let trace = record_outcome(
            &client,
            "q",
            "insufficient_context",
            &[],
            &[],
            "q-no-ent",
        )
        .await;
        assert!(trace.miss_recorded);
        assert_eq!(trace.graph_edges, 0);
        assert!(handle.calls().is_empty());
    }

    #[tokio::test]
    async fn graph_unreachable_returns_graph_ok_false() {
        let _env = TestEnv::new();
        let (client, _) =
            mock::new_with_responder(|_| Reply::err_msg("pipe_connect", "no service"));
        let trace = record_outcome(
            &client,
            "did it work",
            "ok",
            &["foo".into()],
            &["chunk-1".into()],
            "q1",
        )
        .await;
        assert_eq!(trace.graph_edges, 0);
        assert!(!trace.graph_ok);
    }
}
