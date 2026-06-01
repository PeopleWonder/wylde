//! `rag.*` — active RAG tools (Phase 7.B-3).
//!
//! Eight model-callable tools wired against [`crate::memory::rag`]:
//!
//! * `rag_ask` — vector top-K + optional graph expansion. Requires a
//!   precomputed `query_vector` until the wylde-ollama embedder lands;
//!   without one the handler returns `insufficient_context`.
//! * `rag_index` / `rag_reindex` — N8N webhook trigger over HTTP (see
//!   `crate::memory::rag::ingest`).
//! * `rag_prune` — filtered destructive cleanup with dry-run by default.
//! * `rag_feedback` — record ±1/0 rating for a prior `rag_ask` query.
//! * `rag_misses` — list recent retrieval misses.
//! * `rag_chunk_usage` — per-chunk retrieval counts.
//! * `rag_graph_stats` — node/edge counts in the Memgraph service.
//!
//! Mirrors the eight Python manifests under
//! `Core/harness/tooling/tools/rag/` field-for-field on the parameter
//! list so the model sees identical schemas.

use serde_json::json;

use crate::memory::rag::actions;
use crate::tooling::registry::{entry_active, param, param_default, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "rag_ask",
        "rag.ask",
        "rag",
        "Retrieve answer-grounding chunks for a natural-language question from the active \
         workspace index. The Rust port returns ranked vector hits when a precomputed \
         `query_vector` is supplied; without it the call returns \
         status=insufficient_context (the embedder lives in the Python harness until the \
         wylde-ollama Rust port lands).",
        vec![
            param("q", "string", true, "Question to answer"),
            param_default("limit", "number", "Max chunks returned (clamped 1..50)", json!(8)),
            param("query_vector", "array", false, "Precomputed embedding (until embed wired)"),
            param("workspace", "string", false, "Workspace id"),
            param("tier", "string", false, "Restrict to one tier (core/episodic/semantic/procedural)"),
        ],
        false,
        |args, _| async move { actions::run_rag_ask(args).await },
    ));

    reg.insert(entry_active(
        "rag_index",
        "rag.index",
        "rag",
        "Trigger an incremental indexing run over one or more source paths. Unchanged files \
         are skipped by the ingest workflow. Runs asynchronously via N8N — returns the \
         webhook trigger envelope.",
        vec![
            param("paths", "array", false, "Paths relative to target_path (e.g. ['core'])"),
            param("target_path", "string", false, "Workspace root the ingest workflow walks"),
            param_default("workspace_id", "string", "Logical workspace bucket", json!("default")),
            param_default("force", "boolean", "Re-index every file even if unchanged", json!(false)),
        ],
        true,
        |args, _| async move { actions::run_rag_index(args).await },
    ));

    reg.insert(entry_active(
        "rag_reindex",
        "rag.reindex",
        "rag",
        "Wipe the existing index and rebuild from scratch. Heavier than rag_index — only \
         use when the index is structurally stale (chunker/embedder change, schema migration, \
         etc.). Runs asynchronously via N8N.",
        vec![
            param("target_path", "string", false, "Workspace root to rebuild against"),
            param_default("workspace_id", "string", "Logical workspace bucket", json!("default")),
        ],
        true,
        |args, _| async move { actions::run_rag_reindex(args).await },
    ));

    reg.insert(entry_active(
        "rag_prune",
        "rag.prune",
        "rag",
        "Delete memories from the vector store matching the supplied filters. At least one \
         filter (before_ts, memory_type, score_lt) is required. Without confirm=true the tool \
         dry-runs and reports what would be deleted.",
        vec![
            param_default("confirm", "boolean", "Must be true to actually delete", json!(false)),
            param("before_ts", "number", false, "Delete memories created before this unix timestamp"),
            param("memory_type", "string", false, "Delete only memories of this tier"),
            param("score_lt", "number", false, "Delete memories whose score is strictly below this value"),
            param_default("max_delete", "number", "Safety cap (clamped 1..10000)", json!(500)),
        ],
        true,
        |args, _| async move { actions::run_rag_prune(args).await },
    ));

    reg.insert(entry_active(
        "rag_feedback",
        "rag.feedback",
        "rag",
        "Attach user feedback (+1 helpful, 0 neutral, -1 bad) to a prior rag_ask query_id. \
         Persisted via the miss_log memory layer; returns whether the feedback was recorded.",
        vec![
            param("query_id", "string", true, "query_id returned by a prior rag_ask call"),
            param("score", "number", true, "-1, 0, or 1"),
            param("comment", "string", false, "Optional free-text rationale"),
        ],
        false,
        |args, _| async move { actions::run_rag_feedback(args).await },
    ));

    reg.insert(entry_active(
        "rag_misses",
        "rag.misses",
        "rag",
        "List recent queries where retrieval missed (confidence gate fired or no valid \
         citations). Reads from the miss_log memory layer.",
        vec![
            param_default("limit", "number", "Max rows (clamped 1..1000)", json!(100)),
            param_default("only_gated", "boolean", "Restrict to gated rows", json!(true)),
            param_default("include_trace", "boolean", "Include retrieval-trace JSON", json!(false)),
            param("since", "number", false, "Only include rows with ts >= this value"),
        ],
        false,
        |args, _| async move { actions::run_rag_misses(args).await },
    ));

    reg.insert(entry_active(
        "rag_chunk_usage",
        "rag.chunk_usage",
        "rag",
        "Per-chunk retrieval counts from the miss_log memory layer. The layer tracks \
         retrieval frequency only; dead_only is preserved on the surface but only returns \
         rows whose counter is exactly zero.",
        vec![
            param_default("dead_only", "boolean", "If true, return only chunks with zero citations", json!(false)),
            param_default("limit", "number", "Max rows (clamped 1..10000)", json!(100)),
        ],
        false,
        |args, _| async move { actions::run_rag_chunk_usage(args).await },
    ));

    reg.insert(entry_active(
        "rag_graph_stats",
        "rag.graph_stats",
        "rag",
        "Report node/edge counts (entities, chunks, mentions) in the Memgraph service. Safe \
         to call when the graph backend is unreachable — returns reachable=false with zeros \
         rather than raising.",
        vec![],
        false,
        |args, _| async move { actions::run_rag_graph_stats(args).await },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_eight_rag_tools_register_under_canonical_and_alias_keys() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "rag_ask",
            "rag_index",
            "rag_reindex",
            "rag_prune",
            "rag_feedback",
            "rag_misses",
            "rag_chunk_usage",
            "rag_graph_stats",
        ] {
            assert!(reg.lookup(id).is_some(), "missing canonical id {id}");
        }
        for name in [
            "rag.ask",
            "rag.index",
            "rag.reindex",
            "rag.prune",
            "rag.feedback",
            "rag.misses",
            "rag.chunk_usage",
            "rag.graph_stats",
        ] {
            assert!(reg.lookup(name).is_some(), "missing dotted name {name}");
        }
    }

    #[test]
    fn destructive_classification_matches_python_manifests() {
        let mut reg = Registry::empty();
        register(&mut reg);
        // Destructive: index, reindex, prune.
        assert!(reg.lookup("rag_index").unwrap().destructive);
        assert!(reg.lookup("rag_reindex").unwrap().destructive);
        assert!(reg.lookup("rag_prune").unwrap().destructive);
        // Read-only: ask, feedback, misses, chunk_usage, graph_stats.
        assert!(!reg.lookup("rag_ask").unwrap().destructive);
        assert!(!reg.lookup("rag_feedback").unwrap().destructive);
        assert!(!reg.lookup("rag_misses").unwrap().destructive);
        assert!(!reg.lookup("rag_chunk_usage").unwrap().destructive);
        assert!(!reg.lookup("rag_graph_stats").unwrap().destructive);
    }
}
