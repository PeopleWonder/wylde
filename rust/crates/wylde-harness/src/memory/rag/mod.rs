//! `rag/` — tiered semantic memory + retrieval pipeline (Phase 7.B-3).
//!
//! Rust port of `Core/harness/memory/rag.py` + `vector_store.py` +
//! `miss_log.py` + `rag_feedback.py` + `ingest.py`. Wires the eight
//! `rag.*` tools the Python harness exposes through the in-process tool
//! registry.
//!
//! ## Sub-modules
//!
//! * [`store`] — JSON-authoritative tiered records + bincode vector
//!   mirror. Sibling of `crate::memory::long_term::store`, scoped to the
//!   four RAG tiers (`core`, `episodic`, `semantic`, `procedural`).
//! * [`tiers`] — tier vocabulary + string ↔ enum bridges.
//! * [`search`] — vector top-K (`search`), search-and-log
//!   (`search_logged`), and the hybrid vector+graph composer
//!   (`search_with_graph`) shared with `meta.graph_query`.
//! * [`merge`] — `_merge_and_rank` fusion of vector and graph hits.
//! * [`miss_log`] — append-only telemetry for queries, feedback events,
//!   and per-chunk retrieval counters.
//! * [`prune`] — filtered destructive cleanup of the tiered store
//!   (`before_ts` / `memory_type` / `score_lt`).
//! * [`feedback`] — terminal-outcome graph feedback (CITED_IN +
//!   RETRIEVAL_MISS edges) reused by the `rag_feedback` flow.
//! * [`ingest`] — N8N webhook trigger for `rag_index` / `rag_reindex`.
//!   Transport-deferred — see the module's docs for the rationale.
//! * [`actions`] — model-callable handlers wrapping the above for the
//!   eight `rag.*` tool ids.
//!
//! ## Strangler-fig
//!
//! Like every memory submodule, this one is gated by
//! `WYLDE_HARNESS_MEMORY_IMPL` (defaults to `python`). The Rust
//! handlers are reachable through the in-process tool catalog, but the
//! Python `Core/harness/memory/rag.py` (and the LLM-driven
//! `rag_pipeline.py`) stay canonical at runtime until a parity test
//! confirms identical envelopes and the Wylde user flips the impl flag.

pub mod actions;
pub mod feedback;
pub mod ingest;
pub mod merge;
pub mod miss_log;
pub mod prune;
pub mod search;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tiers;

pub use feedback::{record_outcome, OutcomeTrace, MISS_SENTINEL};
pub use merge::{merge_and_rank, AGREEMENT_BONUS, COMBINED_ALPHA};
pub use prune::{prune_rows, PruneError, PruneFilters};
pub use search::{search, search_logged, search_with_graph, Hit, HybridResult, SearchError};
pub use store::{TierHit, TierRecord, TieredStore};
pub use tiers::{
    is_known_tier, tier_from_str, Tier, ALL_TIERS, TIER_CORE, TIER_EPISODIC, TIER_PROCEDURAL,
    TIER_SEMANTIC,
};
