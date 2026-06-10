//! `workspace/` — Layer 2: workspace-scoped, durable memory tier.
//!
//! Rust port of `Core/harness/memory/workspace_memory/` (full-Rust
//! cutover slice R2a). Serves the six `memory.workspace.*` pipe verbs
//! natively; wire shapes match the Python `_memory.py` handlers
//! exactly (the gateway depends on them).
//!
//! ## On-disk layout
//!
//! Mirrors the Python paths byte-for-byte so existing stores are read
//! as-is on cutover:
//!
//! ```text
//! <data_dir>/workspace_memories/<workspace_id>/memory.json  ← source of truth
//! ```
//!
//! The JSON file is the authoritative record list
//! (`{"memories": [...]}`, tmp+rename atomic writes, lenient loading).
//! It lives **outside** the per-workspace file-index folder so MRU
//! eviction of the index never takes the curated memories with it;
//! only an explicit user delete removes it (see
//! [`store::delete_memory_dir`]).
//!
//! ## Design decisions (vs. the Python implementation)
//!
//! * **Search is text-only in this slice.** Python mirrored each
//!   record into a per-workspace LanceDB table and ran vector search
//!   over it. The Rust crate cannot read those `.lance` folders, so on
//!   cutover every existing workspace would have an empty vector
//!   mirror and a vector-only search would silently return nothing.
//!   Instead [`store::search_records`] scores the live JSON records
//!   directly with a query-token-overlap similarity and re-ranks with
//!   the shared importance + recency-decay formula
//!   (`crate::memory::long_term::combined_score` — the Wylde user's
//!   `similarity * importance * exp(-age_days / decay)`). This matches
//!   the crate's existing reality: the Rust `memory.long_term.save`
//!   pipe path doesn't embed either, so nothing populates a workspace
//!   vector mirror today. The embedding bridge
//!   ([`crate::memory::embeddings`]) and the pure-Rust
//!   [`crate::memory::vector::VectorStore`] both exist, so a later
//!   slice can add a `memory.vec.bin` mirror behind the same
//!   `search_records` signature without touching the wire shape.
//! * **Importance scoring is reused, not duplicated.**
//!   `crate::memory::long_term::normalize_importance` is the existing
//!   port of `Core/harness/memory/scoring.py::normalize_importance`
//!   (clamp 1..=10, length+entity heuristic fallback capped at 8);
//!   this module imports it.
//! * **Entity → graph edges are best-effort.** Python's
//!   `_record_entities` lazy-imported memgraph and swallowed every
//!   failure; the Rust port fire-and-forgets a Bolt `upsert` on a
//!   spawned task (see `actions::record_entities_best_effort`) so a
//!   down/slow Neo4j can never block or fail the save path.
//! * **Curation returns the Python parity shape.** The pipe verb can't
//!   inject a chat function across the wire, so `memory.workspace.curate`
//!   always answers with the skipped [`CurationResult`] — exactly what
//!   the Python action returned. The real LLM keep/supersede/merge
//!   pass lands with the scheduler slice via [`curate_with_chat`].
//!
//! ## Submodules
//!
//! * [`record`]  — [`WorkspaceMemory`] + lenient `from_dict`-style decode.
//! * [`store`]   — JSON store: load/save/list/get/save_new/update/delete
//!   + text search. Mirrors `_store.py` (update is
//!   revision-not-deletion; delete sweeps superseded predecessors).
//! * [`actions`] — the six `memory.workspace.*` pipe handlers.

mod record;

pub mod actions;
pub mod store;

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

pub use record::WorkspaceMemory;
pub use store::{
    delete, delete_memory_dir, get, json_path, list_records, memory_dir, save_new,
    search_records, update, workspace_memories_dir, SaveError, SearchHit,
};

/// Records per curator-LLM batch. Mirrors Python's
/// `_curate.CURATION_BATCH_SIZE`. Consumed once the real curation pass
/// lands with the scheduler slice; kept here so the tunable survives
/// the port.
pub const CURATION_BATCH_SIZE: usize = 20;

/// Per-pass curation verdict summary. Mirrors Python's
/// `_curate.CurationResult` — [`CurationResult::to_value`] is the exact
/// `to_dict()` wire shape the `memory.workspace.curate` action returns.
#[derive(Debug, Clone, PartialEq)]
pub struct CurationResult {
    pub workspace_id: String,
    pub inputs_considered: usize,
    /// Record ids the curator chose to keep.
    pub kept: Vec<String>,
    /// `{old_id, reason}` entries for soft-deleted records.
    pub superseded: Vec<Value>,
    /// `{new_id, old_ids, reason}` entries for merge writes.
    pub merged: Vec<Value>,
    pub skipped: bool,
    pub skip_reason: String,
}

impl CurationResult {
    /// A pass that did nothing — the only shape this slice produces.
    pub fn skipped(workspace_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            inputs_considered: 0,
            kept: Vec::new(),
            superseded: Vec::new(),
            merged: Vec::new(),
            skipped: true,
            skip_reason: reason.into(),
        }
    }

    /// JSON wire shape — matches Python `CurationResult.to_dict()`.
    pub fn to_value(&self) -> Value {
        json!({
            "workspace_id": self.workspace_id,
            "inputs_considered": self.inputs_considered,
            "kept": self.kept,
            "superseded": self.superseded,
            "merged": self.merged,
            "skipped": self.skipped,
            "skip_reason": self.skip_reason,
        })
    }
}

/// Boxed future a [`CurateChatFn`] returns: the curator model's raw
/// text (one JSON verdict per line) or a transport error message.
pub type CurateChatFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

/// Chat callback the scheduler slice injects to run a real curation
/// pass: takes the curator `messages` (OpenAI-style `{role, content}`
/// JSON objects, system + user) and returns the model's raw reply
/// text. The pipe action always passes `None` — a chat function isn't
/// injectable across the wire, mirroring Python.
pub type CurateChatFn = dyn Fn(Vec<Value>) -> CurateChatFuture + Send + Sync;

/// LLM-driven curation entry point — Rust port of `_curate.curate`.
///
/// Guard paths mirror Python exactly:
///
/// * empty `workspace_id`  → skipped, `"empty workspace_id"`.
/// * `chat_fn` is `None`   → skipped, `"no chat_fn supplied"` (the
///   pipe-action path — chat functions don't cross the wire).
/// * no live records       → skipped, `"no records to curate"`.
///
/// The batched keep/supersede/merge verdict loop is NOT ported in this
/// slice — only the scheduler (R2b) will ever supply a `chat_fn`, so
/// the live branch currently answers with an honest skipped result
/// rather than a partial pass. The signature is final; the scheduler
/// slice fills in the loop (batching by [`CURATION_BATCH_SIZE`],
/// supersession via tombstone ids, merge via [`store::save_new`]).
pub async fn curate_with_chat(
    workspace_id: &str,
    chat_fn: Option<&CurateChatFn>,
) -> CurationResult {
    if workspace_id.is_empty() {
        return CurationResult::skipped("", "empty workspace_id");
    }
    let Some(_chat) = chat_fn else {
        return CurationResult::skipped(workspace_id, "no chat_fn supplied");
    };
    let records = store::list_records(workspace_id, false);
    if records.is_empty() {
        return CurationResult::skipped(workspace_id, "no records to curate");
    }
    tracing::warn!(
        "workspace_memory: curate_with_chat called with a chat_fn but the \
         verdict loop is not ported yet (lands with the scheduler slice)"
    );
    CurationResult::skipped(workspace_id, "curation pass not yet ported to Rust")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;

    #[tokio::test]
    async fn curate_empty_workspace_id_skips_with_python_reason() {
        let r = curate_with_chat("", None).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "empty workspace_id");
        assert_eq!(r.workspace_id, "");
        assert_eq!(r.inputs_considered, 0);
    }

    #[tokio::test]
    async fn curate_without_chat_fn_skips_with_python_reason() {
        let r = curate_with_chat("ws1", None).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "no chat_fn supplied");
        assert_eq!(r.workspace_id, "ws1");
    }

    /// Stand-in for the scheduler-injected chat function — shape-checks
    /// the [`CurateChatFn`] signature without a live model.
    fn chat_stub(_msgs: Vec<Value>) -> CurateChatFuture {
        Box::pin(async { Ok(String::new()) })
    }

    #[tokio::test]
    async fn curate_with_chat_fn_but_no_records_skips() {
        let _env = TestEnv::new();
        let chat: &CurateChatFn = &chat_stub;
        let r = curate_with_chat("ws_empty", Some(chat)).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "no records to curate");
    }

    #[test]
    fn curation_result_to_value_matches_python_to_dict_shape() {
        let r = CurationResult::skipped("ws1", "no chat_fn supplied");
        let v = r.to_value();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "inputs_considered",
                "kept",
                "merged",
                "skip_reason",
                "skipped",
                "superseded",
                "workspace_id",
            ]
        );
        assert_eq!(v["skipped"], true);
        assert_eq!(v["inputs_considered"], 0);
        assert!(v["kept"].as_array().unwrap().is_empty());
        assert!(v["superseded"].as_array().unwrap().is_empty());
        assert!(v["merged"].as_array().unwrap().is_empty());
    }
}
