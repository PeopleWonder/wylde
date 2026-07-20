//! JSON store + text search for workspace memory. Rust port of
//! `Core/harness/memory/workspace_memory/_store.py` (+ the retrieval
//! half of `_search.py`).
//!
//! ## Source of truth
//!
//! `<data_dir>/workspace_memories/<workspace_id>/memory.json`, shaped
//! `{"memories": [...]}` — the exact file the Python implementation
//! wrote, including soft-deleted / superseded entries for audit walks.
//! Writes are tmp+rename atomic; loading is lenient (unreadable file →
//! empty list, non-list `memories` → empty list, per-item decode via
//! [`WorkspaceMemory::from_value_lenient`] with Python's defaults).
//!
//! Python kept a second store per workspace — a LanceDB vector mirror —
//! which the Rust crate cannot read. This slice replaces it with
//! [`search_records`]: a query-token-overlap similarity over the live
//! JSON records, re-ranked by the shared importance + recency-decay
//! formula. See the module docs in [`super`] for the full rationale;
//! the signature leaves room for a pure-Rust vector mirror later.
//!
//! ## Concurrency
//!
//! A single process-wide mutex serialises every read-modify-write on
//! the JSON files (matches Python's `threading.RLock`; holds are short
//! — JSON IO is tiny and synchronous).
//!
//! ## Update semantics
//!
//! `update` is revision-not-deletion: it writes a NEW record with a
//! new id and points the original's `superseded_by` at it — BOTH stay
//! on disk. `delete` removes the record AND every record whose
//! `superseded_by` names it (the audit predecessors), matching Python.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde_json::{json, Value};

use super::record::WorkspaceMemory;
use crate::memory::common::{data_dir, embed_dim, ensure_dir};
use crate::memory::long_term::{combined_score, normalize_importance, DEFAULT_DECAY_DAYS};
use crate::memory::vector::VectorStore;

/// Process-wide guard over the per-workspace JSON files. One lock for
/// the whole tier (not per-workspace) — matches Python's single
/// `threading.RLock` and keeps the code free of a lock registry.
static STORE_LOCK: Mutex<()> = Mutex::new(());

// ── Storage paths (durable — outside the index folder) ────────────────

/// `<data_dir>/workspace_memories/` — root of the durable tier. NOT
/// under `indexes/`, so MRU eviction of a file index never removes the
/// memories. Mirrors Python's `WORKSPACE_MEMORIES_DIR`.
pub fn workspace_memories_dir() -> PathBuf {
    data_dir().join("workspace_memories")
}

/// Per-workspace durable memory directory.
pub fn memory_dir(workspace_id: &str) -> PathBuf {
    workspace_memories_dir().join(workspace_id)
}

/// `<data_dir>/workspace_memories/<workspace_id>/memory.json`.
pub fn json_path(workspace_id: &str) -> PathBuf {
    memory_dir(workspace_id).join("memory.json")
}

/// `<data_dir>/workspace_memories/<workspace_id>/memory.vec.bin` — the
/// pure-Rust vector mirror (same [`VectorStore`] format the long-term
/// tier uses). Populated by the async save/update handlers via
/// [`vector_upsert`]; consumed by [`search_records_vector`]. Lives
/// beside `memory.json` and, like it, survives MRU eviction of the file
/// index. Absent / empty → semantic search cleanly falls back to the
/// text-overlap [`search_records`].
pub fn vector_path(workspace_id: &str) -> PathBuf {
    memory_dir(workspace_id).join("memory.vec.bin")
}

/// Whether `workspace_id` is safe to interpolate into a durable-tier path.
///
/// [`memory_dir`] is `workspace_memories_dir().join(workspace_id)`, and
/// `Path::join` has two behaviours that turn a hostile id into a path outside
/// the tier:
///
///   * an **empty** id joins to the tier ROOT — so `delete_memory_dir("")`
///     would `remove_dir_all` *every* workspace's memories, not one;
///   * an **absolute** id (`C:\Windows`, `/etc`) DISCARDS the base entirely,
///     aiming the removal at that path;
///   * a **relative traversal** (`../../x`) walks up out of the tier.
///
/// Ids are derived slugs in practice, but the tier is reachable over the pipe
/// (`memory.workspace.*`), so the destructive path validates rather than
/// trusts. Accepts the slug alphabet only: no separators, no `..`, non-empty.
fn is_safe_workspace_id(workspace_id: &str) -> bool {
    !workspace_id.is_empty()
        && workspace_id == workspace_id.trim()
        && workspace_id != ".."
        && workspace_id != "."
        && !workspace_id.contains("..")
        && !workspace_id.contains(['/', '\\', ':'])
}

/// Recursively remove the durable workspace memory folder. Invoked on
/// explicit user delete of a workspace — MRU eviction must NOT call
/// this. Returns `true` if a folder was removed. Mirrors Python's
/// `delete_memory_dir`.
///
/// Refuses an id that would escape the tier (see [`is_safe_workspace_id`]);
/// this is a `remove_dir_all`, so a bad id is a wipe, not a mistake.
pub fn delete_memory_dir(workspace_id: &str) -> bool {
    if !is_safe_workspace_id(workspace_id) {
        tracing::warn!(
            "workspace_memory: refusing to delete memory dir for unsafe \
             workspace id {workspace_id:?}"
        );
        return false;
    }
    let target = memory_dir(workspace_id);
    if !target.exists() {
        return false;
    }
    match std::fs::remove_dir_all(&target) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                "workspace_memory: failed to delete durable memory dir {}: {}",
                target.display(),
                e
            );
            false
        }
    }
}

// ── JSON IO ───────────────────────────────────────────────────────────

fn load_all(workspace_id: &str) -> Vec<WorkspaceMemory> {
    let path = json_path(workspace_id);
    if !path.exists() {
        return Vec::new();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("workspace_memory: {} JSON unreadable: {}", workspace_id, e);
            return Vec::new();
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("workspace_memory: {} JSON unreadable: {}", workspace_id, e);
            return Vec::new();
        }
    };
    // Accept both `{"memories": [...]}` (Python's shape) and a bare
    // array; anything else reads as empty. Matches `_load`.
    let items = if v.is_object() {
        v.get("memories").cloned().unwrap_or(Value::Null)
    } else {
        v
    };
    let Some(arr) = items.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(WorkspaceMemory::from_value_lenient)
        .collect()
}

fn save_all(workspace_id: &str, records: &[WorkspaceMemory]) -> std::io::Result<()> {
    let path = json_path(workspace_id);
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let payload = json!({
        "memories": records.iter().map(WorkspaceMemory::to_value).collect::<Vec<_>>(),
    });
    let body = serde_json::to_string_pretty(&payload).expect("serialise records");
    // Same tmp name Python's `path.with_suffix(".json.tmp")` produced.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 16-char lowercase hex id from 8 random bytes — mirrors Python's
/// `secrets.token_hex(8)`.
fn new_id() -> String {
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

// ── Public read surface ───────────────────────────────────────────────

/// All records for a workspace, sorted importance desc then recency
/// desc. Hides superseded records unless `include_superseded`.
pub fn list_records(workspace_id: &str, include_superseded: bool) -> Vec<WorkspaceMemory> {
    let mut records = {
        let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        load_all(workspace_id)
    };
    if !include_superseded {
        records.retain(|r| r.superseded_by.is_empty());
    }
    records.sort_by(|a, b| {
        b.importance.cmp(&a.importance).then_with(|| {
            b.last_used_at
                .partial_cmp(&a.last_used_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    records
}

/// Lookup by record id. `None` if no match.
pub fn get(workspace_id: &str, record_id: &str) -> Option<WorkspaceMemory> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    load_all(workspace_id)
        .into_iter()
        .find(|r| r.id == record_id)
}

// ── Public write surface ──────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("body must be a non-empty string")]
    EmptyBody,
    #[error("workspace_id is required")]
    EmptyWorkspaceId,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Write a new workspace-scoped memory. Importance is normalised via
/// the shared `normalize_importance` (numeric → clamp 1..=10; missing /
/// non-numeric → length+entity heuristic capped at 8). The body is
/// stored trimmed, matching Python's `body.strip()`.
pub fn save_new(
    workspace_id: &str,
    body: &str,
    source: &str,
    importance: Option<f64>,
    entities: Vec<String>,
) -> Result<WorkspaceMemory, SaveError> {
    let body_trimmed = body.trim();
    if body_trimmed.is_empty() {
        return Err(SaveError::EmptyBody);
    }
    if workspace_id.is_empty() {
        return Err(SaveError::EmptyWorkspaceId);
    }
    let importance_int = normalize_importance(importance, body_trimmed, entities.len());
    let now = now_secs();
    let record = WorkspaceMemory {
        id: new_id(),
        workspace_id: workspace_id.to_owned(),
        body: body_trimmed.to_owned(),
        source: source.to_owned(),
        importance: importance_int,
        created_at: now,
        last_used_at: now,
        superseded_by: String::new(),
        entities,
    };
    {
        let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut records = load_all(workspace_id);
        records.push(record.clone());
        save_all(workspace_id, &records)?;
    }
    tracing::info!(
        "workspace_memory: saved {} in {} (importance={}, entities={})",
        record.id,
        workspace_id,
        record.importance,
        record.entities.len()
    );
    Ok(record)
}

/// Revision-not-deletion: write a NEW record and mark the old one
/// `superseded_by` the new id — both stay on disk. Returns the
/// replacement, or `None` if `record_id` doesn't exist. A blank /
/// missing `body` keeps the original body; a supplied `importance`
/// re-normalises against the (possibly new) body; `entities = None`
/// carries the original list forward. `source` always carries forward.
pub fn update(
    workspace_id: &str,
    record_id: &str,
    body: Option<&str>,
    importance: Option<f64>,
    entities: Option<Vec<String>>,
) -> Option<WorkspaceMemory> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut records = load_all(workspace_id);
    let original_idx = records.iter().position(|r| r.id == record_id)?;
    let original = records[original_idx].clone();

    // Python: `body if isinstance(body, str) and body.strip() else
    // original.body` — the raw (unstripped) body is kept when non-blank.
    let new_body = body
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| original.body.clone());
    let new_importance = match importance {
        Some(_) => normalize_importance(importance, &new_body, 0),
        None => original.importance,
    };
    let new_entities = entities.unwrap_or_else(|| original.entities.clone());

    let now = now_secs();
    let replacement = WorkspaceMemory {
        id: new_id(),
        workspace_id: workspace_id.to_owned(),
        body: new_body,
        source: original.source.clone(),
        importance: new_importance,
        created_at: now,
        last_used_at: now,
        superseded_by: String::new(),
        entities: new_entities,
    };
    records[original_idx].superseded_by = replacement.id.clone();
    records.push(replacement.clone());
    if let Err(e) = save_all(workspace_id, &records) {
        tracing::warn!("workspace_memory: save_all failed during update: {}", e);
        return None;
    }
    Some(replacement)
}

/// Surgically point `old_id`'s `superseded_by` at `new_id` WITHOUT
/// writing a replacement record — the supersession hook curation and
/// reflection use once they already hold the successor (Python
/// `_curate._link_supersession` / `reflection._link_supersession_ws`).
/// `new_id` may be a real record id (merge) or a `tombstone:` marker
/// (soft delete). Returns `true` when the record was found and the
/// write landed.
pub fn link_supersession(workspace_id: &str, old_id: &str, new_id: &str) -> bool {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut records = load_all(workspace_id);
    let Some(rec) = records.iter_mut().find(|r| r.id == old_id) else {
        return false;
    };
    rec.superseded_by = new_id.to_owned();
    match save_all(workspace_id, &records) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                "workspace_memory: save_all failed during link_supersession: {}",
                e
            );
            false
        }
    }
}

/// Bump a record's `last_used_at` (M5 — the dedup path re-warms the
/// existing insight instead of minting a duplicate). No-op for an
/// unknown id.
pub fn touch(workspace_id: &str, record_id: &str) {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut records = load_all(workspace_id);
    let Some(rec) = records.iter_mut().find(|r| r.id == record_id) else {
        return;
    };
    rec.last_used_at = now_secs();
    if let Err(e) = save_all(workspace_id, &records) {
        tracing::warn!("workspace_memory: save_all failed during touch: {}", e);
    }
}

/// Permanently remove a record AND every record whose `superseded_by`
/// names it (its audit predecessors). Returns `true` if anything was
/// deleted.
pub fn delete(workspace_id: &str, record_id: &str) -> bool {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let records = load_all(workspace_id);
    if !records.iter().any(|r| r.id == record_id) {
        return false;
    }
    let mut ids: HashSet<&str> = HashSet::new();
    ids.insert(record_id);
    for r in &records {
        if r.superseded_by == record_id {
            ids.insert(r.id.as_str());
        }
    }
    let deleted_ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    let remaining: Vec<WorkspaceMemory> = records
        .iter()
        .filter(|r| !ids.contains(r.id.as_str()))
        .cloned()
        .collect();
    if let Err(e) = save_all(workspace_id, &remaining) {
        tracing::warn!("workspace_memory: save_all failed during delete: {}", e);
        return false;
    }
    // Keep the vector mirror in sync — drop every removed id.
    for id in &deleted_ids {
        vector_delete(workspace_id, id);
    }
    true
}

// ── Vector mirror ─────────────────────────────────────────────────────
//
// A per-workspace [`VectorStore`] (`memory.vec.bin`) mirrors the JSON
// records' embeddings so search can rank semantically instead of by bare
// token overlap. It is best-effort: populated by the async save/update
// handlers ([`super::actions`]) which have the embedder; direct sync
// callers (curation merges, some reflection paths) may leave records
// un-mirrored, so [`search_records_vector`] joins strictly against the
// live JSON and the action layer merges its hits with the text baseline
// to preserve recall.

fn vector_store(workspace_id: &str) -> VectorStore {
    VectorStore::load_or_empty(&vector_path(workspace_id), embed_dim())
}

fn persist_vector_store(workspace_id: &str, store: &VectorStore) {
    let path = vector_path(workspace_id);
    if let Some(parent) = path.parent() {
        if let Err(e) = ensure_dir(parent) {
            tracing::warn!("workspace_memory: failed to ensure vector parent: {}", e);
            return;
        }
    }
    if let Err(e) = store.persist(&path) {
        tracing::warn!("workspace_memory: vector persist failed: {}", e);
    }
}

/// Upsert one record's embedding into the workspace vector mirror.
/// `None` is a no-op (the caller couldn't embed — text search still
/// covers the record). Best-effort: a persist failure logs and leaves
/// the record un-mirrored.
pub fn vector_upsert(workspace_id: &str, record_id: &str, vector: Option<Vec<f32>>) {
    let Some(vec) = vector else { return };
    let mut store = vector_store(workspace_id);
    if let Err(e) = store.insert(record_id, vec) {
        tracing::warn!(
            "workspace_memory: vector upsert failed for {}/{}: {}",
            workspace_id,
            record_id,
            e
        );
        return;
    }
    persist_vector_store(workspace_id, &store);
}

/// Remove one record from the workspace vector mirror. No-op if absent.
pub fn vector_delete(workspace_id: &str, record_id: &str) {
    let mut store = vector_store(workspace_id);
    if store.delete(record_id) {
        persist_vector_store(workspace_id, &store);
    }
}

/// True when the mirror holds no vectors — the signal the action layer
/// uses to skip the vector path entirely and stay on text.
pub fn vector_mirror_is_empty(workspace_id: &str) -> bool {
    vector_store(workspace_id).is_empty()
}

/// Vector search over a workspace's live (non-superseded) records, then
/// re-rank by importance + recency decay — the semantic sibling of
/// [`search_records`]. The caller embeds the query (this module stays
/// embedder-free on the read path, mirroring `long_term::search`).
/// Records absent from the mirror simply don't appear here; the action
/// layer merges these hits with the text baseline so recall never
/// regresses. Empty query vector / zero limit → empty.
pub fn search_records_vector(
    workspace_id: &str,
    query_vector: Vec<f32>,
    limit: usize,
    decay_days: Option<f64>,
) -> Vec<SearchHit> {
    if query_vector.is_empty() || limit == 0 {
        return Vec::new();
    }
    let store = vector_store(workspace_id);
    // Over-fetch to leave headroom for the supersession filter + re-rank,
    // matching the long-term store's `max(limit * 4, 16)`.
    let k = std::cmp::max(limit * 4, 16);
    let hits = match store.query_topk(query_vector, k) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                "workspace_memory: vector search failed for {}: {}",
                workspace_id,
                e
            );
            return Vec::new();
        }
    };
    let by_id: std::collections::HashMap<String, WorkspaceMemory> =
        list_records(workspace_id, false)
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
    let decay = decay_days.unwrap_or(DEFAULT_DECAY_DAYS);
    let mut out: Vec<SearchHit> = Vec::new();
    for h in hits {
        let Some(rec) = by_id.get(&h.id) else {
            continue; // superseded / deleted since the mirror last wrote
        };
        let similarity = h.similarity as f64;
        let score = combined_score(
            similarity,
            rec.importance as f64,
            rec.last_used_at,
            decay,
            None,
        );
        out.push(SearchHit {
            id: rec.id.clone(),
            body: rec.body.clone(),
            source: rec.source.clone(),
            importance: rec.importance,
            created_at: rec.created_at,
            last_used_at: rec.last_used_at,
            similarity,
            workspace_id: workspace_id.to_owned(),
            score,
        });
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(limit);
    out
}

/// Merge a semantic hit list with the text-overlap baseline, keeping the
/// higher-scoring hit per record id, then re-sort (score desc, id asc)
/// and truncate to `limit`. This is how the action layer gets semantic
/// ranking for mirrored records without losing recall on records the
/// mirror doesn't yet cover.
pub fn merge_hits(
    vector_hits: Vec<SearchHit>,
    text_hits: Vec<SearchHit>,
    limit: usize,
) -> Vec<SearchHit> {
    use std::collections::HashMap;
    let mut best: HashMap<String, SearchHit> = HashMap::new();
    for hit in vector_hits.into_iter().chain(text_hits) {
        match best.get(&hit.id) {
            Some(existing) if existing.score >= hit.score => {}
            _ => {
                best.insert(hit.id.clone(), hit);
            }
        }
    }
    let mut merged: Vec<SearchHit> = best.into_values().collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    merged.truncate(limit);
    merged
}

// ── Search ────────────────────────────────────────────────────────────

/// One scored search hit. [`SearchHit::to_value`] is the exact
/// candidate shape Python's `_search.search` returned after
/// `rank_by_score` annotated it: the record fields plus `similarity`,
/// `workspace_id`, and the combined `score`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub body: String,
    pub source: String,
    pub importance: i32,
    pub created_at: f64,
    pub last_used_at: f64,
    pub similarity: f64,
    pub workspace_id: String,
    pub score: f64,
}

impl SearchHit {
    /// JSON hit shape — matches the Python search candidate dict.
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "body": self.body,
            "source": self.source,
            "importance": self.importance,
            "created_at": self.created_at,
            "last_used_at": self.last_used_at,
            "similarity": self.similarity,
            "workspace_id": self.workspace_id,
            "score": self.score,
        })
    }
}

/// Text search over the live (non-superseded) records of a workspace.
///
/// Similarity is the fraction of distinct query tokens present in the
/// record body (case-insensitive, alphanumeric token runs) — a value
/// in `[0, 1]`, so it slots into the shared scoring formula exactly
/// where Python's cosine similarity did:
///
/// ```text
/// score = similarity * (importance / 10) * exp(-age_days / decay)
/// ```
///
/// Records with zero overlap are not hits. Results sort score desc
/// (ties broken by id asc for determinism) and truncate to `limit`.
/// An empty / whitespace-only query returns an empty list, matching
/// the Python search's silent `[]`.
///
/// This is deliberately text-only — see the module docs in [`super`]
/// for why the Python LanceDB vector mirror was not carried over.
pub fn search_records(
    workspace_id: &str,
    query: &str,
    limit: usize,
    decay_days: Option<f64>,
) -> Vec<SearchHit> {
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() || limit == 0 {
        return Vec::new();
    }
    let records = list_records(workspace_id, false);
    let decay = decay_days.unwrap_or(DEFAULT_DECAY_DAYS);
    let mut hits: Vec<SearchHit> = Vec::new();
    for r in records {
        let body_tokens = tokenize(&r.body);
        let overlap = query_tokens.intersection(&body_tokens).count();
        if overlap == 0 {
            continue;
        }
        let similarity = overlap as f64 / query_tokens.len() as f64;
        let score = combined_score(similarity, r.importance as f64, r.last_used_at, decay, None);
        hits.push(SearchHit {
            id: r.id,
            body: r.body,
            source: r.source,
            importance: r.importance,
            created_at: r.created_at,
            last_used_at: r.last_used_at,
            similarity,
            workspace_id: workspace_id.to_owned(),
            score,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    hits
}

/// Lowercased alphanumeric token runs. `"Build-Watcher v2"` →
/// `{"build", "watcher", "v2"}`.
fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;

    #[test]
    fn save_new_persists_and_fills_id_and_timestamps() {
        let _env = TestEnv::new();
        let r = save_new(
            "ws1",
            "  hello world  ",
            "chat",
            Some(7.0),
            vec!["e1".into()],
        )
        .unwrap();
        assert_eq!(r.id.len(), 16);
        assert!(r
            .id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(r.body, "hello world"); // stored trimmed
        assert_eq!(r.workspace_id, "ws1");
        assert_eq!(r.source, "chat");
        assert_eq!(r.importance, 7);
        assert!(r.created_at > 0.0);
        assert_eq!(r.entities, vec!["e1".to_string()]);

        let back = get("ws1", &r.id).unwrap();
        assert_eq!(back.body, r.body);
        assert_eq!(back.importance, r.importance);
        assert_eq!(back.entities, r.entities);
    }

    #[test]
    fn save_new_rejects_blank_body_and_empty_workspace() {
        let _env = TestEnv::new();
        assert!(matches!(
            save_new("ws1", "   ", "", None, vec![]),
            Err(SaveError::EmptyBody)
        ));
        assert!(matches!(
            save_new("", "body", "", None, vec![]),
            Err(SaveError::EmptyWorkspaceId)
        ));
    }

    #[test]
    fn save_new_uses_heuristic_importance_when_none() {
        let _env = TestEnv::new();
        // length_pts=0, entity_pts=2 → 3 + 0 + 2 = 5.
        let r = save_new("ws1", "short", "", None, vec!["a".into(), "b".into()]).unwrap();
        assert_eq!(r.importance, 5);
    }

    #[test]
    fn list_records_sorts_importance_desc_then_recency_desc() {
        let _env = TestEnv::new();
        let a = save_new("ws1", "aaa", "", Some(3.0), vec![]).unwrap();
        let b = save_new("ws1", "bbb", "", Some(8.0), vec![]).unwrap();
        let c = save_new("ws1", "ccc", "", Some(5.0), vec![]).unwrap();
        let lst = list_records("ws1", false);
        assert_eq!(
            lst.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec![b.id.as_str(), c.id.as_str(), a.id.as_str()]
        );
    }

    #[test]
    fn workspaces_are_isolated_from_each_other() {
        let _env = TestEnv::new();
        save_new("ws_a", "alpha body", "", Some(5.0), vec![]).unwrap();
        save_new("ws_b", "beta body", "", Some(5.0), vec![]).unwrap();
        assert_eq!(list_records("ws_a", true).len(), 1);
        assert_eq!(list_records("ws_b", true).len(), 1);
        assert_eq!(list_records("ws_a", true)[0].body, "alpha body");
    }

    #[test]
    fn update_supersedes_original_and_keeps_both_on_disk() {
        let _env = TestEnv::new();
        let orig = save_new("ws1", "original", "src", Some(6.0), vec!["x".into()]).unwrap();
        let rev = update("ws1", &orig.id, Some("revised"), Some(8.0), None).unwrap();
        assert_ne!(orig.id, rev.id);
        assert_eq!(rev.body, "revised");
        assert_eq!(rev.importance, 8);
        assert_eq!(rev.source, "src"); // carried forward
        assert_eq!(rev.entities, vec!["x".to_string()]); // carried forward
        assert!(rev.superseded_by.is_empty());

        // Original still on disk, superseded.
        let fetched = get("ws1", &orig.id).unwrap();
        assert_eq!(fetched.superseded_by, rev.id);

        // Default list hides it; include_superseded surfaces both.
        assert_eq!(list_records("ws1", false).len(), 1);
        assert_eq!(list_records("ws1", true).len(), 2);
    }

    #[test]
    fn update_blank_body_keeps_original_body() {
        let _env = TestEnv::new();
        let orig = save_new("ws1", "keep me", "", Some(5.0), vec![]).unwrap();
        let rev = update("ws1", &orig.id, Some("   "), None, None).unwrap();
        assert_eq!(rev.body, "keep me");
        assert_eq!(rev.importance, 5);
    }

    #[test]
    fn update_replaces_entities_only_when_supplied() {
        let _env = TestEnv::new();
        let orig = save_new("ws1", "body", "", Some(5.0), vec!["old".into()]).unwrap();
        let rev = update("ws1", &orig.id, None, None, Some(vec!["new".into()])).unwrap();
        assert_eq!(rev.entities, vec!["new".to_string()]);
        // Explicit empty list clears them (Python `entities is not None`).
        let rev2 = update("ws1", &rev.id, None, None, Some(vec![])).unwrap();
        assert!(rev2.entities.is_empty());
    }

    #[test]
    fn update_unknown_id_returns_none() {
        let _env = TestEnv::new();
        assert!(update("ws1", "no-such", Some("x"), None, None).is_none());
    }

    #[test]
    fn delete_removes_record_and_its_superseded_predecessors() {
        let _env = TestEnv::new();
        let v1 = save_new("ws1", "v1", "", Some(5.0), vec![]).unwrap();
        let v2 = update("ws1", &v1.id, Some("v2"), None, None).unwrap();
        assert!(delete("ws1", &v2.id));
        assert!(get("ws1", &v1.id).is_none(), "predecessor swept up");
        assert!(get("ws1", &v2.id).is_none());
    }

    #[test]
    fn link_supersession_sets_pointer_without_new_record() {
        let _env = TestEnv::new();
        let r = save_new("ws1", "victim", "", Some(5.0), vec![]).unwrap();
        assert!(link_supersession(
            "ws1",
            &r.id,
            "tombstone:deadbeefdeadbeef"
        ));
        let stored = get("ws1", &r.id).unwrap();
        assert_eq!(stored.superseded_by, "tombstone:deadbeefdeadbeef");
        // No replacement record was minted; default list hides it.
        assert_eq!(list_records("ws1", true).len(), 1);
        assert!(list_records("ws1", false).is_empty());
        // Unknown ids report false.
        assert!(!link_supersession("ws1", "no-such", "x"));
    }

    #[test]
    fn delete_unknown_id_returns_false() {
        let _env = TestEnv::new();
        assert!(!delete("ws1", "no-such-id"));
    }

    #[test]
    fn delete_memory_dir_removes_folder_and_reports_absence() {
        let _env = TestEnv::new();
        save_new("ws1", "body", "", Some(5.0), vec![]).unwrap();
        assert!(json_path("ws1").exists());
        assert!(delete_memory_dir("ws1"));
        assert!(!memory_dir("ws1").exists());
        assert!(!delete_memory_dir("ws1"));
    }

    /// `delete_memory_dir` is a `remove_dir_all`, and `Path::join` will
    /// happily leave the tier for an empty, absolute, or traversing id — an
    /// empty id resolves to the tier ROOT (every workspace's memories), and an
    /// absolute one discards the base and aims the removal wherever it points.
    /// Now that the pipe can reach this path (#135), a hostile id must be
    /// refused rather than obeyed.
    #[test]
    fn delete_memory_dir_refuses_ids_that_escape_the_tier() {
        let _env = TestEnv::new();
        // Two real workspaces' memories, which must survive every attempt.
        save_new("ws1", "keep me", "", Some(5.0), vec![]).unwrap();
        save_new("ws2", "keep me too", "", Some(5.0), vec![]).unwrap();
        assert!(json_path("ws1").exists());
        assert!(json_path("ws2").exists());

        // An empty id resolves to the tier root — the whole-store wipe.
        assert!(!delete_memory_dir(""), "empty id must be refused");
        assert!(!delete_memory_dir("   "), "blank id must be refused");
        // Traversal out of the tier.
        assert!(!delete_memory_dir(".."), "'..' must be refused");
        assert!(
            !delete_memory_dir("../../somewhere"),
            "traversal must be refused"
        );
        // An absolute id discards the base entirely.
        assert!(
            !delete_memory_dir(r"C:\Windows\Temp"),
            "absolute id must be refused"
        );
        assert!(
            !delete_memory_dir("/etc"),
            "absolute posix id must be refused"
        );
        // A separator anywhere is out.
        assert!(
            !delete_memory_dir("ws1/nested"),
            "separator must be refused"
        );

        // Nothing was touched — including the tier root itself.
        assert!(workspace_memories_dir().exists(), "tier root survives");
        assert!(json_path("ws1").exists(), "ws1 memories survive");
        assert!(json_path("ws2").exists(), "ws2 memories survive");

        // ...and a legitimate id still works.
        assert!(delete_memory_dir("ws1"));
        assert!(
            json_path("ws2").exists(),
            "sibling untouched by a real delete"
        );
    }

    #[test]
    fn load_is_lenient_about_garbage_and_wrong_shapes() {
        let _env = TestEnv::new();
        let path = json_path("ws1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // Unparseable JSON → empty.
        std::fs::write(&path, "{not json").unwrap();
        assert!(list_records("ws1", true).is_empty());

        // Non-list `memories` → empty.
        std::fs::write(&path, r#"{"memories": "nope"}"#).unwrap();
        assert!(list_records("ws1", true).is_empty());

        // Non-dict items skipped; dict items decoded leniently.
        std::fs::write(
            &path,
            r#"{"memories": [42, "str", {"id": "ok1", "body": "kept"}]}"#,
        )
        .unwrap();
        let lst = list_records("ws1", true);
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].id, "ok1");
        assert_eq!(lst[0].importance, 5); // default
    }

    #[test]
    fn rust_reads_python_shaped_json_and_writes_same_top_level_key() {
        let _env = TestEnv::new();
        let path = json_path("proj");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let payload = serde_json::json!({
            "memories": [{
                "id": "abc123abc123abc1",
                "workspace_id": "proj",
                "body": "hello from python",
                "source": "py",
                "importance": 8,
                "created_at": 1.0,
                "last_used_at": 2.0,
                "superseded_by": "",
                "entities": ["py"]
            }]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

        let r = get("proj", "abc123abc123abc1").unwrap();
        assert_eq!(r.body, "hello from python");
        assert_eq!(r.importance, 8);

        save_new("proj", "rust write", "rs", Some(5.0), vec![]).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let arr = parsed["memories"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn save_leaves_no_tmp_file_behind() {
        let _env = TestEnv::new();
        save_new("ws1", "body", "", Some(5.0), vec![]).unwrap();
        let tmp = json_path("ws1").with_extension("json.tmp");
        assert!(!tmp.exists(), "tmp file left behind: {tmp:?}");
        assert!(json_path("ws1").exists());
    }

    // ── search_records ───────────────────────────────────────────────

    #[test]
    fn search_ranks_full_token_overlap_above_partial() {
        let _env = TestEnv::new();
        let full = save_new("ws1", "rust harness pipe actions", "", Some(5.0), vec![]).unwrap();
        let partial = save_new("ws1", "rust toolchain notes", "", Some(5.0), vec![]).unwrap();
        let hits = search_records("ws1", "rust harness", 5, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, full.id);
        assert_eq!(hits[1].id, partial.id);
        assert!(hits[0].similarity > hits[1].similarity);
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[0].workspace_id, "ws1");
    }

    #[test]
    fn search_importance_breaks_equal_similarity() {
        let _env = TestEnv::new();
        let _low = save_new("ws1", "deploy checklist", "", Some(2.0), vec![]).unwrap();
        let high = save_new("ws1", "deploy runbook", "", Some(9.0), vec![]).unwrap();
        let hits = search_records("ws1", "deploy", 5, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, high.id);
    }

    #[test]
    fn search_excludes_superseded_and_zero_overlap_records() {
        let _env = TestEnv::new();
        let v1 = save_new("ws1", "shared topic v1", "", Some(5.0), vec![]).unwrap();
        let v2 = update("ws1", &v1.id, Some("shared topic v2"), None, None).unwrap();
        save_new("ws1", "completely unrelated", "", Some(9.0), vec![]).unwrap();
        let hits = search_records("ws1", "shared topic", 5, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, v2.id);
    }

    #[test]
    fn search_truncates_to_limit() {
        let _env = TestEnv::new();
        for i in 0..5 {
            save_new(
                "ws1",
                &format!("common token row{i}"),
                "",
                Some(5.0),
                vec![],
            )
            .unwrap();
        }
        assert_eq!(search_records("ws1", "common", 2, None).len(), 2);
    }

    #[test]
    fn search_empty_or_whitespace_query_returns_empty() {
        let _env = TestEnv::new();
        save_new("ws1", "body", "", Some(5.0), vec![]).unwrap();
        assert!(search_records("ws1", "", 5, None).is_empty());
        assert!(search_records("ws1", "  \t ", 5, None).is_empty());
    }

    #[test]
    fn search_is_case_insensitive() {
        let _env = TestEnv::new();
        let r = save_new("ws1", "Build Watcher polls Outputs", "", Some(5.0), vec![]).unwrap();
        let hits = search_records("ws1", "build WATCHER", 5, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, r.id);
        assert!((hits[0].similarity - 1.0).abs() < 1e-9);
    }

    // ── vector mirror + hybrid search ────────────────────────────────

    fn set_embed_dim_3() {
        std::env::set_var("WYLDE_EMBED_DIM", "3");
    }

    #[test]
    fn vector_mirror_empty_by_default_and_after_delete() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        assert!(vector_mirror_is_empty("wsm"));
        let a = save_new("wsm", "body", "", Some(5.0), vec![]).unwrap();
        vector_upsert("wsm", &a.id, Some(vec![1.0, 0.0, 0.0]));
        assert!(!vector_mirror_is_empty("wsm"));
        assert!(!vector_path("wsm").as_os_str().is_empty());
        // delete prunes the mirror in lockstep.
        assert!(delete("wsm", &a.id));
        assert!(vector_mirror_is_empty("wsm"));
    }

    #[test]
    fn vector_upsert_none_is_a_noop() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let a = save_new("wsn", "body", "", Some(5.0), vec![]).unwrap();
        vector_upsert("wsn", &a.id, None);
        assert!(
            vector_mirror_is_empty("wsn"),
            "None must not create a mirror"
        );
    }

    #[test]
    fn vector_search_ranks_by_cosine_and_skips_superseded() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let near = save_new("wsv", "near record", "", Some(5.0), vec![]).unwrap();
        let far = save_new("wsv", "far record", "", Some(5.0), vec![]).unwrap();
        vector_upsert("wsv", &near.id, Some(vec![1.0, 0.0, 0.0]));
        vector_upsert("wsv", &far.id, Some(vec![0.0, 1.0, 0.0]));

        let hits = search_records_vector("wsv", vec![1.0, 0.0, 0.0], 5, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, near.id, "closest cosine ranks first");
        assert!(hits[0].similarity > hits[1].similarity);
        assert_eq!(hits[0].workspace_id, "wsv");

        // Supersede `near`; its replacement is not mirrored, so the
        // now-superseded original must drop out of the vector results.
        let _rev = update("wsv", &near.id, Some("near v2"), None, None).unwrap();
        let hits2 = search_records_vector("wsv", vec![1.0, 0.0, 0.0], 5, None);
        assert!(
            hits2.iter().all(|h| h.id != near.id),
            "superseded original filtered from vector hits"
        );
    }

    #[test]
    fn vector_search_empty_query_or_zero_limit_returns_empty() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let a = save_new("wsz", "body", "", Some(5.0), vec![]).unwrap();
        vector_upsert("wsz", &a.id, Some(vec![1.0, 0.0, 0.0]));
        assert!(search_records_vector("wsz", vec![], 5, None).is_empty());
        assert!(search_records_vector("wsz", vec![1.0, 0.0, 0.0], 0, None).is_empty());
    }

    #[test]
    fn merge_hits_keeps_higher_score_per_id_and_sorts() {
        let mk = |id: &str, score: f64| SearchHit {
            id: id.to_owned(),
            body: String::new(),
            source: String::new(),
            importance: 5,
            created_at: 0.0,
            last_used_at: 0.0,
            similarity: 0.0,
            workspace_id: "w".to_owned(),
            score,
        };
        // `a` appears in both lists — the higher (vector, 0.9) wins.
        let vector_hits = vec![mk("a", 0.9), mk("b", 0.2)];
        let text_hits = vec![mk("a", 0.3), mk("c", 0.5)];
        let merged = merge_hits(vector_hits, text_hits, 10);
        assert_eq!(
            merged.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "c", "b"]
        );
        assert_eq!(merged.iter().filter(|h| h.id == "a").count(), 1);
        assert!((merged[0].score - 0.9).abs() < 1e-9);
        // Truncation honours the limit.
        assert_eq!(
            merge_hits(vec![mk("a", 0.9)], vec![mk("b", 0.5)], 1).len(),
            1
        );
    }

    #[test]
    fn search_hit_to_value_has_python_candidate_keys() {
        let _env = TestEnv::new();
        save_new("ws1", "wire shape check", "src", Some(5.0), vec![]).unwrap();
        let hits = search_records("ws1", "wire", 5, None);
        let v = hits[0].to_value();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "body",
                "created_at",
                "id",
                "importance",
                "last_used_at",
                "score",
                "similarity",
                "source",
                "workspace_id",
            ]
        );
    }
}
