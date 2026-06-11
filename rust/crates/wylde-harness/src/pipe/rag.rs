//! `rag.*` pipe registrations (Wylde_Study S2a) -- episodic write +
//! semantic search. Split from `pipe.rs` per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_RAG: &str = "wylde_harness::api::DefaultHarnessApi (rag.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── rag.* (Wylde_Study S2a) ──────────────────────────────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "rag.add_episodic",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.rag_add_episodic(p).await }
        },
        "Add one raw-text episodic memory row (the Rust port of \
         rag.add_episodic). Writes to the same tiered RAG store \
         rag.search reads, so the row is immediately retrievable. \
         Payload: {content|text, source_path?|url?, session_id?, \
         score?, vector?}. Embeds `content` via wylde-ollama when no \
         `vector` is supplied. Returns {status, memory_id, id, chars, \
         memory_type}.",
        HANDLER_MODULE_RAG,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "rag.search",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.rag_search(p).await }
        },
        "Semantic search over the tiered RAG store. Embeds the query \
         text server-side via wylde-ollama (unlike the model-callable \
         rag.ask tool, which requires a precomputed vector), then runs \
         the same first-party vector search. Payload: {q, query_vector?, \
         limit?, tier?, workspace?}. Returns {status, q, workspace_id, \
         results, count}.",
        HANDLER_MODULE_RAG,
    );
}
