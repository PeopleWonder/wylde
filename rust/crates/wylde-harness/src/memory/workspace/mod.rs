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
//! * **Curation is fully ported (slice R2b).** [`curate_with_chat`]
//!   runs the Python `_curate.py` keep / supersede / merge verdict
//!   loop against an injected [`ReflectionChat`] — the crate's shared
//!   chat abstraction (reused per the R2b directive; it replaced the
//!   caller-less `CurateChatFn` placeholder from R2a). The
//!   `memory.workspace.curate` pipe verb still answers the skipped
//!   [`CurationResult`] (a chat function can't cross the wire — Python
//!   parity); real passes run in-process via the `memory.reflect`
//!   workspace scope and the background scheduler
//!   ([`crate::memory::scheduler`]).
//!
//! ## Submodules
//!
//! * [`record`]  — [`WorkspaceMemory`] + lenient `from_dict`-style decode.
//! * [`store`]   — JSON store: load/save/list/get/save_new/update/delete
//!   + text search. Mirrors `_store.py` (update is
//!     revision-not-deletion; delete sweeps superseded predecessors).
//! * [`actions`] — the six `memory.workspace.*` pipe handlers.

mod record;

pub mod actions;
pub mod store;

use std::collections::HashSet;

use rand::RngCore;
use serde_json::{json, Value};

use crate::memory::long_term::reflection::ReflectionChat;

pub use record::WorkspaceMemory;
pub use store::{
    delete, delete_memory_dir, get, json_path, link_supersession, list_records, memory_dir,
    save_new, search_records, update, workspace_memories_dir, SaveError, SearchHit,
};

/// Records per curator-LLM batch. Mirrors Python's
/// `_curate.CURATION_BATCH_SIZE`.
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

/// System message for the curator LLM. Verbatim from
/// `_curate.py::_CURATION_SYSTEM` so the cutover doesn't change model
/// behaviour.
pub const CURATION_SYSTEM_PROMPT: &str = "You are a memory curator. You read a list of memory records about \
a project and decide which are still relevant for ongoing work and \
which are stale, redundant, or no longer important.\n\n\
Output ONE JSON object per line, no preamble, no trailing text. \
Each object refers to one input by its 1-based index and carries \
a verdict. Three verdict shapes:\n  \
{\"index\": 3, \"verdict\": \"keep\"}\n  \
{\"index\": 5, \"verdict\": \"supersede\", \"reason\": \"<why>\"}\n  \
{\"index\": 7, \"verdict\": \"merge\", \"into\": [3, 8], \"new_body\": \"<consolidated paragraph>\", \"reason\": \"<why>\"}\n\n\
Rules:\n\
* `keep` — the memory is still useful as-is.\n\
* `supersede` — the memory is no longer relevant. The reason field \
is required. The memory will be soft-deleted (still in history).\n\
* `merge` — combine multiple memories into one new entry. List the \
input indices in `into` (must include the current index). Provide \
the consolidated `new_body`. The originals will be marked superseded.\n\n\
Default to `keep` when uncertain. Be conservative — when in doubt, \
keep the memory.";

/// Prefix a supersede verdict's tombstone id carries — a soft-delete
/// with audit trail (the original stays on disk for history walks but
/// is hidden from default retrieval). Mirrors Python
/// `_curate._TOMBSTONE_PREFIX`.
pub const TOMBSTONE_PREFIX: &str = "tombstone:";

/// LLM-driven curation entry point — Rust port of `_curate.curate`
/// with the Python defaults (no model override, batches of
/// [`CURATION_BATCH_SIZE`]).
///
/// Guard paths mirror Python exactly:
///
/// * empty `workspace_id`  → skipped, `"empty workspace_id"`.
/// * `chat` is `None`      → skipped, `"no chat_fn supplied"` (the
///   pipe-action path — chat functions don't cross the wire).
/// * no live records       → skipped, `"no records to curate"`.
///
/// The chat surface is the crate-wide [`ReflectionChat`] trait (one
/// messages-in / text-out turn) — the same abstraction the reflection
/// cycles inject — replacing the placeholder `CurateChatFn` alias this
/// module shipped in slice R2a (it had no callers).
pub async fn curate_with_chat(
    workspace_id: &str,
    chat: Option<&dyn ReflectionChat>,
) -> CurationResult {
    curate_with_options(workspace_id, chat, None, CURATION_BATCH_SIZE).await
}

/// [`curate_with_chat`] with the model + batch-size knobs Python's
/// `curate(..., model=..., batch_size=...)` exposed.
pub async fn curate_with_options(
    workspace_id: &str,
    chat: Option<&dyn ReflectionChat>,
    model: Option<String>,
    batch_size: usize,
) -> CurationResult {
    if workspace_id.is_empty() {
        return CurationResult::skipped("", "empty workspace_id");
    }
    let Some(chat) = chat else {
        return CurationResult::skipped(workspace_id, "no chat_fn supplied");
    };
    let records = store::list_records(workspace_id, false);
    if records.is_empty() {
        return CurationResult::skipped(workspace_id, "no records to curate");
    }

    let mut result = CurationResult {
        workspace_id: workspace_id.to_owned(),
        inputs_considered: records.len(),
        kept: Vec::new(),
        superseded: Vec::new(),
        merged: Vec::new(),
        skipped: false,
        skip_reason: String::new(),
    };

    // Python's transport-failure path returned None verdicts and
    // continued with the next batch; the Rust chat trait folds failures
    // to an empty reply, which parses to zero verdicts — same effect.
    for batch in records.chunks(batch_size.max(1)) {
        let verdicts = ask_curator(chat, batch, model.clone()).await;
        apply_verdicts(workspace_id, batch, &verdicts, &mut result);
    }

    tracing::info!(
        "workspace_memory: curated {} — {} kept, {} superseded, {} merged",
        workspace_id,
        result.kept.len(),
        result.superseded.len(),
        result.merged.len()
    );
    result
}

/// Format one batch, ask the curator LLM, parse its line-per-verdict
/// reply. Mirrors `_ask_curator`.
async fn ask_curator(
    chat: &dyn ReflectionChat,
    batch: &[WorkspaceMemory],
    model: Option<String>,
) -> Vec<Value> {
    let lines: Vec<String> = batch
        .iter()
        .enumerate()
        .map(|(i, m)| {
            format!(
                "{}. (importance {}, id {}) {}",
                i + 1,
                m.importance,
                m.id,
                m.body
            )
        })
        .collect();
    let messages = vec![
        json!({"role": "system", "content": CURATION_SYSTEM_PROMPT}),
        json!({"role": "user", "content": lines.join("\n")}),
    ];
    let raw = chat.ask(messages, model).await;
    parse_verdict_lines(&raw)
}

/// One JSON object per line. Blank lines and code-fence markers are
/// tolerated; unparseable lines and objects without an `index` are
/// dropped. Public for tests; mirrors the parsing half of
/// `_ask_curator`.
pub fn parse_verdict_lines(raw: &str) -> Vec<Value> {
    let mut verdicts: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.is_object() && obj.get("index").is_some() {
            verdicts.push(obj);
        }
    }
    verdicts
}

/// Mutate the workspace memory store per the LLM's verdicts. Mirrors
/// `_apply_verdicts`:
///
/// * `keep`      — append to `result.kept`; no store mutation.
/// * `supersede` — point the original's `superseded_by` at a fresh
///   tombstone id (default list / search hide it; history keeps it).
/// * `merge`     — write a new merged record, link every cited
///   original's `superseded_by` to the new id, union the entities.
fn apply_verdicts(
    workspace_id: &str,
    batch: &[WorkspaceMemory],
    verdicts: &[Value],
    result: &mut CurationResult,
) {
    for verdict in verdicts {
        let Some(target) = index_to_record(batch, verdict.get("index")) else {
            continue;
        };
        match verdict_kind(verdict).as_str() {
            "keep" => result.kept.push(target.id.clone()),
            "supersede" => {
                let reason = string_or(verdict.get("reason"), "curated as stale");
                let tombstone_id = format!("{TOMBSTONE_PREFIX}{}", token_hex8());
                store::link_supersession(workspace_id, &target.id, &tombstone_id);
                result
                    .superseded
                    .push(json!({"old_id": target.id, "reason": reason}));
            }
            "merge" => {
                let Some(into) = verdict.get("into").and_then(Value::as_array) else {
                    continue;
                };
                let old_ids: Vec<String> = into
                    .iter()
                    .filter_map(|j| index_to_record(batch, Some(j)))
                    .map(|r| r.id.clone())
                    .collect();
                let new_body = verdict
                    .get("new_body")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| target.body.clone());
                if old_ids.is_empty() || new_body.trim().is_empty() {
                    continue;
                }
                // Importance: max of the merged inputs (a synthesis is
                // at least as heavy as its heaviest input).
                let new_importance = old_ids
                    .iter()
                    .filter_map(|oid| batch.iter().find(|r| &r.id == oid))
                    .map(|r| r.importance)
                    .max()
                    .unwrap_or(target.importance);
                // Union the entity sets (first-seen order) so the
                // consolidation keeps the graph edges.
                let mut seen: HashSet<String> = HashSet::new();
                let mut ent_union: Vec<String> = Vec::new();
                for oid in &old_ids {
                    if let Some(rec) = batch.iter().find(|r| &r.id == oid) {
                        for e in &rec.entities {
                            if seen.insert(e.clone()) {
                                ent_union.push(e.clone());
                            }
                        }
                    }
                }
                let source = format!("curation:merge from {}", old_ids.join(","));
                let new_record = match store::save_new(
                    workspace_id,
                    &new_body,
                    &source,
                    Some(f64::from(new_importance)),
                    ent_union,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("workspace_memory: curation merge save failed: {e}");
                        continue;
                    }
                };
                let reason = string_or(verdict.get("reason"), "merged by curator");
                for oid in &old_ids {
                    store::link_supersession(workspace_id, oid, &new_record.id);
                }
                result.merged.push(json!({
                    "new_id": new_record.id,
                    "old_ids": old_ids,
                    "reason": reason,
                }));
            }
            // Unknown verdict kinds fall through untouched — Python's
            // if-chain did the same.
            _ => {}
        }
    }
}

/// Resolve a verdict's 1-based `index` against the batch. Numeric
/// strings are accepted (Python coerced int-or-str); anything else —
/// missing, float-ish, unparseable, out of range — resolves to `None`
/// and the verdict is dropped.
fn index_to_record<'a>(
    batch: &'a [WorkspaceMemory],
    idx: Option<&Value>,
) -> Option<&'a WorkspaceMemory> {
    let i: i64 = match idx? {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    usize::try_from(i.checked_sub(1)?)
        .ok()
        .and_then(|i| batch.get(i))
}

/// `verdict.get("verdict") or verdict.get("action") or "keep"` — the
/// Python falsy chain.
fn verdict_kind(verdict: &Value) -> String {
    for key in ["verdict", "action"] {
        if let Some(s) = verdict.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return s.to_owned();
            }
        }
    }
    "keep".to_owned()
}

/// `str(value or default)` — non-empty strings pass through, truthy
/// non-strings render as JSON, falsy values take the default.
fn string_or(v: Option<&Value>, default: &str) -> String {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Bool(true)) => "true".to_owned(),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
        Some(Value::Array(a)) if !a.is_empty() => Value::Array(a.clone()).to_string(),
        Some(Value::Object(o)) if !o.is_empty() => Value::Object(o.clone()).to_string(),
        _ => default.to_owned(),
    }
}

/// 16-char lowercase hex from 8 random bytes — Python's
/// `secrets.token_hex(8)` (used for tombstone ids).
fn token_hex8() -> String {
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;
    use std::sync::Mutex;

    /// Mock curator returning a fixed reply, recording each call's
    /// messages so batching tests can count LLM turns.
    struct FixedChat {
        reply: String,
        calls: Mutex<Vec<Vec<Value>>>,
    }

    impl FixedChat {
        fn new(reply: impl Into<String>) -> Self {
            FixedChat {
                reply: reply.into(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ReflectionChat for FixedChat {
        async fn ask(&self, messages: Vec<Value>, _model: Option<String>) -> String {
            self.calls.lock().unwrap().push(messages);
            self.reply.clone()
        }
    }

    // ── guard paths (Python parity) ──────────────────────────────────

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

    #[tokio::test]
    async fn curate_with_chat_fn_but_no_records_skips() {
        let _env = TestEnv::new();
        let chat = FixedChat::new("");
        let r = curate_with_chat("ws_empty", Some(&chat)).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "no records to curate");
        assert!(
            chat.calls.lock().unwrap().is_empty(),
            "no LLM call without records"
        );
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

    // ── verdict parsing ──────────────────────────────────────────────

    #[test]
    fn parse_verdicts_tolerates_fences_blanks_and_garbage() {
        let raw = "```json\n\n{\"index\": 1, \"verdict\": \"keep\"}\nnot json at all\n{\"no_index\": true}\n  {\"index\": 2, \"verdict\": \"supersede\", \"reason\": \"old\"}  \n```";
        let v = parse_verdict_lines(raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["index"], 1);
        assert_eq!(v[1]["verdict"], "supersede");
    }

    // ── full verdict application ─────────────────────────────────────

    #[tokio::test]
    async fn curate_applies_keep_supersede_and_merge_verdicts() {
        let _env = TestEnv::new();
        // Distinct importances pin the list order: r1, r2, r3.
        let r1 = save_new("wsc", "alpha fact", "t", Some(9.0), vec!["e1".into()]).unwrap();
        let r2 = save_new("wsc", "beta fact", "t", Some(8.0), vec!["e2".into()]).unwrap();
        let r3 = save_new(
            "wsc",
            "gamma fact",
            "t",
            Some(7.0),
            vec!["e1".into(), "e3".into()],
        )
        .unwrap();

        let chat = FixedChat::new(
            "{\"index\": 2, \"verdict\": \"keep\"}\n\
             {\"index\": 1, \"verdict\": \"supersede\", \"reason\": \"stale\"}\n\
             {\"index\": 3, \"verdict\": \"merge\", \"into\": [1, 3], \"new_body\": \"alpha+gamma merged\", \"reason\": \"dup\"}",
        );
        let result = curate_with_chat("wsc", Some(&chat)).await;
        assert!(!result.skipped);
        assert_eq!(result.inputs_considered, 3);

        // keep
        assert_eq!(result.kept, vec![r2.id.clone()]);

        // supersede → tombstone, hidden from default list.
        assert_eq!(result.superseded.len(), 1);
        assert_eq!(result.superseded[0]["old_id"], r1.id.as_str());
        assert_eq!(result.superseded[0]["reason"], "stale");

        // merge → new record, originals linked to it.
        assert_eq!(result.merged.len(), 1);
        let new_id = result.merged[0]["new_id"].as_str().unwrap().to_owned();
        assert_eq!(
            result.merged[0]["old_ids"],
            json!([r1.id.clone(), r3.id.clone()])
        );
        assert_eq!(result.merged[0]["reason"], "dup");

        let merged_rec = get("wsc", &new_id).expect("merge record present");
        assert_eq!(merged_rec.body, "alpha+gamma merged");
        assert_eq!(merged_rec.importance, 9, "max of merged inputs");
        assert_eq!(
            merged_rec.entities,
            vec!["e1".to_string(), "e3".to_string()]
        );
        assert_eq!(
            merged_rec.source,
            format!("curation:merge from {},{}", r1.id, r3.id)
        );

        // r1 was first tombstoned, then re-linked by the merge — last
        // write wins, exactly like Python's sequential verdict loop.
        assert_eq!(get("wsc", &r1.id).unwrap().superseded_by, new_id);
        assert_eq!(get("wsc", &r3.id).unwrap().superseded_by, new_id);
        assert!(get("wsc", &r2.id).unwrap().superseded_by.is_empty());

        // Default list now shows the keeper + the merge record only.
        let live: Vec<String> = list_records("wsc", false)
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(live.len(), 2);
        assert!(live.contains(&r2.id) && live.contains(&new_id));
        // History view still has everything.
        assert_eq!(list_records("wsc", true).len(), 4);
    }

    #[tokio::test]
    async fn supersede_uses_tombstone_prefix() {
        let _env = TestEnv::new();
        let r1 = save_new("wst", "only", "t", Some(5.0), vec![]).unwrap();
        let chat = FixedChat::new("{\"index\": 1, \"verdict\": \"supersede\"}");
        let result = curate_with_chat("wst", Some(&chat)).await;
        assert_eq!(result.superseded[0]["reason"], "curated as stale");
        let stored = get("wst", &r1.id).unwrap();
        assert!(stored.superseded_by.starts_with(TOMBSTONE_PREFIX));
        assert_eq!(stored.superseded_by.len(), TOMBSTONE_PREFIX.len() + 16);
    }

    #[tokio::test]
    async fn bogus_indices_and_unknown_kinds_are_ignored() {
        let _env = TestEnv::new();
        save_new("wsb", "solo", "t", Some(5.0), vec![]).unwrap();
        let chat = FixedChat::new(
            "{\"index\": 99, \"verdict\": \"keep\"}\n\
             {\"index\": 0, \"verdict\": \"keep\"}\n\
             {\"index\": \"x\", \"verdict\": \"keep\"}\n\
             {\"index\": 1, \"verdict\": \"destroy\"}\n\
             {\"index\": 1, \"verdict\": \"merge\", \"into\": \"not a list\"}\n\
             {\"index\": 1, \"verdict\": \"merge\", \"into\": [99]}",
        );
        let result = curate_with_chat("wsb", Some(&chat)).await;
        assert!(result.kept.is_empty());
        assert!(result.superseded.is_empty());
        assert!(result.merged.is_empty());
        assert_eq!(list_records("wsb", true).len(), 1, "store untouched");
    }

    #[tokio::test]
    async fn missing_verdict_key_defaults_to_keep_and_action_alias_works() {
        let _env = TestEnv::new();
        let r1 = save_new("wsk", "a", "t", Some(6.0), vec![]).unwrap();
        let r2 = save_new("wsk", "b", "t", Some(5.0), vec![]).unwrap();
        let chat = FixedChat::new("{\"index\": 1}\n{\"index\": 2, \"action\": \"keep\"}");
        let result = curate_with_chat("wsk", Some(&chat)).await;
        assert_eq!(result.kept, vec![r1.id, r2.id]);
    }

    #[tokio::test]
    async fn curation_batches_by_batch_size() {
        let _env = TestEnv::new();
        for i in 0..3 {
            save_new("wsbatch", &format!("rec {i}"), "t", Some(5.0), vec![]).unwrap();
        }
        // Each 1-record batch keeps its first (only) record.
        let chat = FixedChat::new("{\"index\": 1, \"verdict\": \"keep\"}");
        let result = curate_with_options("wsbatch", Some(&chat), None, 1).await;
        assert_eq!(result.inputs_considered, 3);
        assert_eq!(result.kept.len(), 3);
        assert_eq!(
            chat.calls.lock().unwrap().len(),
            3,
            "one LLM turn per batch"
        );
    }

    #[tokio::test]
    async fn curator_prompt_carries_system_and_numbered_batch() {
        let _env = TestEnv::new();
        let r1 = save_new("wsp", "the body", "t", Some(5.0), vec![]).unwrap();
        let chat = FixedChat::new("");
        let _ = curate_with_chat("wsp", Some(&chat)).await;
        let calls = chat.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0]["role"], "system");
        assert_eq!(calls[0][0]["content"], CURATION_SYSTEM_PROMPT);
        let user = calls[0][1]["content"].as_str().unwrap();
        assert_eq!(user, format!("1. (importance 5, id {}) the body", r1.id));
    }

    #[tokio::test]
    async fn empty_chat_reply_yields_no_verdicts_but_counts_inputs() {
        let _env = TestEnv::new();
        save_new("wse", "rec", "t", Some(5.0), vec![]).unwrap();
        let chat = FixedChat::new("");
        let result = curate_with_chat("wse", Some(&chat)).await;
        assert!(!result.skipped);
        assert_eq!(result.inputs_considered, 1);
        assert!(result.kept.is_empty() && result.superseded.is_empty() && result.merged.is_empty());
    }
}
