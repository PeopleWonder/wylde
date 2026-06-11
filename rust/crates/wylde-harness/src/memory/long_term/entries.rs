//! Authoritative JSON layer + vector-mirror coordinator for long-term
//! memory. Rust port of `Core/harness/memory/long_term.py`.
//!
//! Public ops match Python: [`list_records`], [`get`], [`save`],
//! [`update`], [`delete`], [`history`], [`search`], [`core_block`],
//! [`touch`].
//!
//! ## Concurrency
//!
//! A single process-wide mutex serialises every read-modify-write on
//! the JSON file. Matches Python's `threading.RLock`. Holds are short —
//! JSON IO is tiny + synchronous.
//!
//! ## Vector handling
//!
//! Each write path also updates the per-process [`VectorStore`]. The
//! store is lazily created from disk on first access and persisted
//! after every mutation. The store is keyed by the same record id as
//! the JSON; `delete` removes from both.
//!
//! Embeddings are **not** computed inside this module — callers pass a
//! pre-embedded vector (or `None`, in which case the record is saved
//! to JSON only and the vector mirror flagged stale on the next
//! reindex pass). This keeps long_term free of an `wylde-ollama` dep
//! while leaving the wire-action layer free to embed before calling.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::records::LongTermMemory;
use super::scoring::{combined_score, normalize_importance, DEFAULT_DECAY_DAYS};
use crate::memory::common::{data_dir, embed_dim, ensure_dir};
use crate::memory::vector::VectorStore;

/// Process-wide guard over the JSON + vector-mirror files.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// `<data_dir>/long_term.json` — authoritative record list.
pub fn json_path() -> PathBuf {
    data_dir().join("long_term.json")
}

/// `<data_dir>/long_term.vec.bin` — pure-Rust vector mirror file. The
/// Python impl writes a `<data_dir>/long_term.lance/` folder instead;
/// the format change is documented in
/// `memory/wylde_phase7b_long_term_shipped.md`.
pub fn vector_path() -> PathBuf {
    data_dir().join("long_term.vec.bin")
}

/// Top-level JSON shape — `{"memories": [...]}`. Matches Python.
#[derive(Debug, Serialize, Deserialize, Default)]
struct OnDisk {
    #[serde(default)]
    memories: Vec<LongTermMemory>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn new_id() -> String {
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

// ── JSON IO ───────────────────────────────────────────────────────────

fn load_all() -> Vec<LongTermMemory> {
    let path = json_path();
    if !path.exists() {
        return Vec::new();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "long_term: JSON unreadable for {}, treating as empty: {}",
                path.display(),
                e
            );
            return Vec::new();
        }
    };
    // Accept both `{"memories": [...]}` (Python's shape) and a bare array
    // (legacy / hand-written files).
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "long_term: JSON parse failed for {}, treating as empty: {}",
                path.display(),
                e
            );
            return Vec::new();
        }
    };
    let items = if v.is_object() {
        v.get("memories").cloned().unwrap_or(Value::Null)
    } else {
        v
    };
    let Some(arr) = items.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|x| serde_json::from_value::<LongTermMemory>(x.clone()).ok())
        .collect()
}

fn save_all(records: &[LongTermMemory]) -> std::io::Result<()> {
    let path = json_path();
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let payload = OnDisk {
        memories: records.to_vec(),
    };
    let json = serde_json::to_string_pretty(&payload).expect("serialise records");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ── Vector mirror ─────────────────────────────────────────────────────

fn vector_store() -> VectorStore {
    VectorStore::load_or_empty(&vector_path(), embed_dim())
}

fn persist_vector_store(store: &VectorStore) {
    let path = vector_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = ensure_dir(parent) {
            tracing::warn!("long_term: failed to ensure vector parent: {}", e);
            return;
        }
    }
    if let Err(e) = store.persist(&path) {
        tracing::warn!("long_term: vector persist failed: {}", e);
    }
}

fn vector_upsert(record_id: &str, vector: Option<Vec<f32>>) {
    let Some(vec) = vector else { return };
    let mut store = vector_store();
    if let Err(e) = store.insert(record_id, vec) {
        tracing::warn!("long_term: vector upsert failed for {}: {}", record_id, e);
        return;
    }
    persist_vector_store(&store);
}

fn vector_delete(record_id: &str) {
    let mut store = vector_store();
    if store.delete(record_id) {
        persist_vector_store(&store);
    }
}

// ── Public read surface ───────────────────────────────────────────────

/// All long-term records, sorted importance desc then recency desc.
/// Hides superseded records unless `include_superseded` is true.
pub fn list_records(include_superseded: bool) -> Vec<LongTermMemory> {
    let _g = STORE_LOCK.lock().unwrap();
    let mut records = load_all();
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
pub fn get(record_id: &str) -> Option<LongTermMemory> {
    let _g = STORE_LOCK.lock().unwrap();
    load_all().into_iter().find(|r| r.id == record_id)
}

/// Top-importance records — the always-in-context block. Defaults to 5
/// when called with `limit = None`.
pub fn core_block(limit: Option<usize>) -> Vec<LongTermMemory> {
    let n = limit.unwrap_or(5);
    let mut records = list_records(false);
    records.truncate(n);
    records
}

// ── Public write surface ──────────────────────────────────────────────

/// Save a new long-term record. `vector`, if supplied, mirrors into the
/// vector store. Returns the new record (with id + timestamps filled).
///
/// Panics if `body` is empty after trim — matches Python's `ValueError`.
pub fn save(
    body: &str,
    source: &str,
    importance: Option<f64>,
    tags: Vec<String>,
    vector: Option<Vec<f32>>,
) -> Result<LongTermMemory, SaveError> {
    let body_trimmed = body.trim();
    if body_trimmed.is_empty() {
        return Err(SaveError::EmptyBody);
    }
    let importance_int = normalize_importance(importance, body_trimmed, tags.len());
    let now = now_secs();
    let record = LongTermMemory {
        id: new_id(),
        body: body_trimmed.to_owned(),
        source: source.to_owned(),
        importance: importance_int,
        created_at: now,
        last_used_at: now,
        superseded_by: String::new(),
        tags,
    };

    {
        let _g = STORE_LOCK.lock().unwrap();
        let mut records = load_all();
        records.push(record.clone());
        save_all(&records).map_err(SaveError::Io)?;
    }
    vector_upsert(&record.id, vector);
    Ok(record)
}

/// Revise an existing record by writing a NEW record and marking the
/// old one `superseded_by` the new id. Returns the new record, or
/// `None` if `record_id` doesn't exist.
pub fn update(
    record_id: &str,
    body: Option<&str>,
    importance: Option<f64>,
    source: Option<&str>,
    vector: Option<Vec<f32>>,
) -> Option<LongTermMemory> {
    let (replacement, original_id) = {
        let _g = STORE_LOCK.lock().unwrap();
        let mut records = load_all();
        let original_idx = records.iter().position(|r| r.id == record_id)?;
        let original = records[original_idx].clone();

        let new_body = body
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| original.body.clone());
        let new_importance = match importance {
            Some(_) => normalize_importance(importance, &new_body, 0),
            None => original.importance,
        };
        let new_source = source
            .map(str::to_owned)
            .unwrap_or_else(|| original.source.clone());

        let now = now_secs();
        let replacement = LongTermMemory {
            id: new_id(),
            body: new_body,
            source: new_source,
            importance: new_importance,
            created_at: now,
            last_used_at: now,
            superseded_by: String::new(),
            tags: original.tags.clone(),
        };
        // Flag the original.
        records[original_idx].superseded_by = replacement.id.clone();
        records.push(replacement.clone());
        if let Err(e) = save_all(&records) {
            tracing::warn!("long_term: save_all failed during update: {}", e);
            return None;
        }
        (replacement, original.id)
    };

    vector_upsert(&replacement.id, vector);
    // The Python impl re-upserts the original to refresh its superseded_by
    // in the lance mirror — for the pure-Rust store that's a no-op since
    // we don't mirror metadata fields, but we still touch it so the
    // similarity search can be filtered by id-set later. No-op for now.
    let _ = original_id;
    Some(replacement)
}

/// Permanently remove a record (and any other records superseded by
/// it). Returns `true` if anything was deleted.
pub fn delete(record_id: &str) -> bool {
    let to_delete = {
        let _g = STORE_LOCK.lock().unwrap();
        let records = load_all();
        let Some(_target) = records.iter().find(|r| r.id == record_id) else {
            return false;
        };
        let mut ids_to_delete: Vec<String> = vec![record_id.to_owned()];
        for r in &records {
            if r.superseded_by == record_id {
                ids_to_delete.push(r.id.clone());
            }
        }
        let remaining: Vec<LongTermMemory> = records
            .into_iter()
            .filter(|r| !ids_to_delete.contains(&r.id))
            .collect();
        if let Err(e) = save_all(&remaining) {
            tracing::warn!("long_term: save_all failed during delete: {}", e);
            return false;
        }
        ids_to_delete
    };
    for rid in &to_delete {
        vector_delete(rid);
    }
    true
}

/// Walk the supersession chain rooted at `record_id`. Returns the
/// chain in oldest-to-newest order; the active record is the last one.
pub fn history(record_id: &str) -> Vec<LongTermMemory> {
    let _g = STORE_LOCK.lock().unwrap();
    let records = load_all();
    let by_id: std::collections::HashMap<String, LongTermMemory> =
        records.iter().map(|r| (r.id.clone(), r.clone())).collect();
    if !by_id.contains_key(record_id) {
        return Vec::new();
    }

    // Walk forward (record → its successor → ...).
    let mut chain: Vec<LongTermMemory> = Vec::new();
    let mut cur = by_id.get(record_id).cloned();
    while let Some(r) = cur {
        let next_id = r.superseded_by.clone();
        chain.push(r);
        if next_id.is_empty() {
            break;
        }
        cur = by_id.get(&next_id).cloned();
    }

    // Walk backward (anything whose superseded_by is the start).
    let mut backward: Vec<LongTermMemory> = Vec::new();
    let mut seek = record_id.to_owned();
    loop {
        let prev = records.iter().find(|r| r.superseded_by == seek).cloned();
        match prev {
            Some(p) => {
                seek = p.id.clone();
                backward.push(p);
            }
            None => break,
        }
    }
    backward.reverse();
    backward.extend(chain);
    backward
}

/// Bump `last_used_at` to now. No-op if the id doesn't exist.
pub fn touch(record_id: &str) {
    let _g = STORE_LOCK.lock().unwrap();
    let mut records = load_all();
    let mut hit = false;
    for r in records.iter_mut() {
        if r.id == record_id {
            r.last_used_at = now_secs();
            hit = true;
            break;
        }
    }
    if hit {
        let _ = save_all(&records);
    }
}

/// Bump `last_used_at` on several records in ONE load/save pass — the
/// per-turn injection path (improvement plan B3) touches every record it
/// puts in the prompt, and N separate [`touch`] calls would rewrite the
/// JSON N times. Unknown ids are skipped.
pub fn touch_all(record_ids: &[String]) {
    if record_ids.is_empty() {
        return;
    }
    let _g = STORE_LOCK.lock().unwrap();
    let mut records = load_all();
    let now = now_secs();
    let mut hit = false;
    for r in records.iter_mut() {
        if record_ids.iter().any(|id| *id == r.id) {
            r.last_used_at = now;
            hit = true;
        }
    }
    if hit {
        let _ = save_all(&records);
    }
}

// ── Search ────────────────────────────────────────────────────────────

/// Hit shape returned by [`search`]. Carries the raw record fields the
/// Python `search()` returns, plus the combined score.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub body: String,
    pub source: String,
    pub importance: i32,
    pub created_at: f64,
    pub last_used_at: f64,
    pub similarity: f64,
    pub score: f64,
}

impl SearchHit {
    /// Convert to a JSON Value matching Python's search() output shape.
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "body": self.body,
            "source": self.source,
            "importance": self.importance,
            "created_at": self.created_at,
            "last_used_at": self.last_used_at,
            "similarity": self.similarity,
            "score": self.score,
        })
    }
}

/// Vector search over long-term, then re-rank by importance + recency
/// decay. Superseded records are filtered out.
///
/// Caller embeds the query externally (this module stays Ollama-free)
/// and supplies the vector. Empty `query_vector` returns an empty list.
pub fn search(query_vector: Vec<f32>, limit: usize, decay_days: Option<f64>) -> Vec<SearchHit> {
    if query_vector.is_empty() || limit == 0 {
        return Vec::new();
    }
    let store = vector_store();
    // Over-fetch by 4x to leave headroom for the supersession + scoring
    // re-rank, matching Python's `max(limit * 4, 16)`.
    let k = std::cmp::max(limit * 4, 16);
    let hits = match store.query_topk(query_vector, k) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("long_term: search query failed: {}", e);
            return Vec::new();
        }
    };

    let records = {
        let _g = STORE_LOCK.lock().unwrap();
        load_all()
    };
    let by_id: std::collections::HashMap<String, LongTermMemory> =
        records.into_iter().map(|r| (r.id.clone(), r)).collect();

    let decay = decay_days.unwrap_or(DEFAULT_DECAY_DAYS);
    let mut out: Vec<SearchHit> = Vec::new();
    for h in hits {
        let Some(rec) = by_id.get(&h.id) else {
            continue;
        };
        if !rec.superseded_by.is_empty() {
            continue;
        }
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
            score,
        });
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

// ── Errors ────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("body must be a non-empty string")]
    EmptyBody,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;

    fn vec3(a: f32, b: f32, c: f32) -> Vec<f32> {
        vec![a, b, c]
    }

    fn set_embed_dim_3() {
        std::env::set_var("WYLDE_EMBED_DIM", "3");
    }

    #[test]
    fn save_persists_and_returns_record_with_id_and_timestamps() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let r = save(
            "hello world",
            "ui",
            Some(7.0),
            vec!["alpha".into()],
            Some(vec3(1.0, 0.0, 0.0)),
        )
        .unwrap();
        assert!(!r.id.is_empty());
        assert_eq!(r.body, "hello world");
        assert_eq!(r.source, "ui");
        assert_eq!(r.importance, 7);
        assert!(r.created_at > 0.0);
        assert_eq!(r.tags, vec!["alpha".to_string()]);

        // Round-trips JSON. Timestamps may drift by a ULP through f64 →
        // string → f64, so compare them with a small tolerance and the
        // rest of the struct by equality.
        let back = get(&r.id).unwrap();
        assert_eq!(back.id, r.id);
        assert_eq!(back.body, r.body);
        assert_eq!(back.source, r.source);
        assert_eq!(back.importance, r.importance);
        assert_eq!(back.superseded_by, r.superseded_by);
        assert_eq!(back.tags, r.tags);
        assert!(
            (back.created_at - r.created_at).abs() < 1e-3,
            "created_at drift: {} vs {}",
            back.created_at,
            r.created_at,
        );
        assert!(
            (back.last_used_at - r.last_used_at).abs() < 1e-3,
            "last_used_at drift: {} vs {}",
            back.last_used_at,
            r.last_used_at,
        );
    }

    #[test]
    fn save_rejects_empty_body() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let err = save("   ", "", None, vec![], None).unwrap_err();
        assert!(matches!(err, SaveError::EmptyBody));
    }

    #[test]
    fn save_uses_heuristic_when_importance_none() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let r = save("hello", "", None, vec![], None).unwrap();
        // length_pts=0, entity_pts=0 → 3
        assert_eq!(r.importance, 3);
    }

    #[test]
    fn list_records_sorts_by_importance_desc_then_recency_desc() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let a = save("aaa", "", Some(3.0), vec![], None).unwrap();
        let b = save("bbb", "", Some(8.0), vec![], None).unwrap();
        let c = save("ccc", "", Some(5.0), vec![], None).unwrap();
        let lst = list_records(false);
        assert_eq!(lst[0].id, b.id);
        assert_eq!(lst[1].id, c.id);
        assert_eq!(lst[2].id, a.id);
    }

    #[test]
    fn list_records_hides_superseded_by_default() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let a = save("aaa", "", Some(5.0), vec![], None).unwrap();
        let _b = update(&a.id, Some("aaa v2"), None, None, None).unwrap();
        let visible = list_records(false);
        assert_eq!(visible.len(), 1);
        assert!(visible[0].body.contains("v2"));
        let all = list_records(true);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn update_creates_new_record_and_supersedes_original() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let orig = save("original", "", Some(6.0), vec!["x".into()], None).unwrap();
        let revised = update(&orig.id, Some("revised"), Some(8.0), None, None).unwrap();
        assert_ne!(orig.id, revised.id);
        assert_eq!(revised.body, "revised");
        assert_eq!(revised.importance, 8);
        // Tags carry forward.
        assert_eq!(revised.tags, vec!["x".to_string()]);
        // Original now has superseded_by = revised.id.
        let fetched_orig = get(&orig.id).unwrap();
        assert_eq!(fetched_orig.superseded_by, revised.id);
    }

    #[test]
    fn update_unknown_id_returns_none() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        assert!(update("no-such", Some("x"), None, None, None).is_none());
    }

    #[test]
    fn delete_removes_record_and_anything_superseding_it() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v1 = save("v1", "", Some(5.0), vec![], None).unwrap();
        let v2 = update(&v1.id, Some("v2"), None, None, None).unwrap();
        assert!(delete(&v2.id));
        // Both ids should be gone (v1's superseded_by points to v2,
        // so v1 is swept up too).
        assert!(get(&v1.id).is_none());
        assert!(get(&v2.id).is_none());
    }

    #[test]
    fn delete_unknown_id_returns_false() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        assert!(!delete("no-such-id"));
    }

    #[test]
    fn history_walks_the_supersession_chain_oldest_to_newest() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v1 = save("v1", "", Some(5.0), vec![], None).unwrap();
        let v2 = update(&v1.id, Some("v2"), None, None, None).unwrap();
        let _v3 = update(&v2.id, Some("v3"), None, None, None).unwrap();
        let chain = history(&v2.id);
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].body, "v1");
        assert_eq!(chain[1].body, "v2");
        assert_eq!(chain[2].body, "v3");
    }

    #[test]
    fn history_returns_empty_for_unknown_id() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        assert!(history("no-such").is_empty());
    }

    #[test]
    fn touch_updates_last_used_at_for_known_id_noop_for_unknown() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let r = save("body", "", Some(5.0), vec![], None).unwrap();
        let before = r.last_used_at;
        // Force a deterministic gap.
        std::thread::sleep(std::time::Duration::from_millis(2));
        touch(&r.id);
        let after = get(&r.id).unwrap();
        assert!(after.last_used_at > before);

        // Unknown id is silently ignored — assert by checking the
        // record list is unchanged.
        let len = list_records(true).len();
        touch("no-such");
        assert_eq!(list_records(true).len(), len);
    }

    #[test]
    fn core_block_returns_top_importance_capped_at_limit() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        for i in 1..=7 {
            save(&format!("b{i}"), "", Some(i as f64), vec![], None).unwrap();
        }
        let block = core_block(Some(3));
        assert_eq!(block.len(), 3);
        // Sorted importance desc.
        assert_eq!(block[0].importance, 7);
        assert_eq!(block[1].importance, 6);
        assert_eq!(block[2].importance, 5);
    }

    #[test]
    fn search_returns_records_ranked_by_combined_score() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // Two records — one closer to query than the other, but the
        // farther one has higher importance.
        let near = save("near", "", Some(3.0), vec![], Some(vec3(1.0, 0.0, 0.0))).unwrap();
        let far_but_important =
            save("far_imp", "", Some(10.0), vec![], Some(vec3(0.0, 1.0, 0.0))).unwrap();
        // Query close to `near`.
        let hits = search(vec3(1.0, 0.0, 0.0), 5, Some(30.0));
        assert_eq!(hits.len(), 2);
        // Both surfaced; ordering depends on combined score.
        // near: similarity 1.0 * 0.3 * exp(~0) ≈ 0.3
        // far : similarity 0.0 * 1.0 * exp(~0) ≈ 0.0
        // So near wins because the cosine of far is 0.
        assert_eq!(hits[0].id, near.id);
        assert_eq!(hits[1].id, far_but_important.id);
    }

    #[test]
    fn search_filters_superseded_records_out() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let v1 = save("v1", "", Some(5.0), vec![], Some(vec3(1.0, 0.0, 0.0))).unwrap();
        let v2 = update(&v1.id, Some("v2"), None, None, Some(vec3(1.0, 0.0, 0.0))).unwrap();
        let hits = search(vec3(1.0, 0.0, 0.0), 5, None);
        // Only v2 should surface.
        assert!(hits.iter().any(|h| h.id == v2.id));
        assert!(!hits.iter().any(|h| h.id == v1.id));
    }

    #[test]
    fn search_returns_empty_for_empty_query_vector() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        save("body", "", Some(5.0), vec![], Some(vec3(1.0, 0.0, 0.0))).unwrap();
        assert!(search(vec![], 5, None).is_empty());
    }

    #[test]
    fn search_returns_empty_for_zero_limit() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        save("body", "", Some(5.0), vec![], Some(vec3(1.0, 0.0, 0.0))).unwrap();
        assert!(search(vec3(1.0, 0.0, 0.0), 0, None).is_empty());
    }

    /// Integration: write a Python-shaped JSON fixture, then read via
    /// the Rust impl, confirm shape parity.
    #[test]
    fn rust_reads_python_shaped_json_round_trip() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let path = json_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let payload = serde_json::json!({
            "memories": [
                {
                    "id": "abc123",
                    "body": "hello from python",
                    "source": "py",
                    "importance": 8,
                    "created_at": 1.0,
                    "last_used_at": 2.0,
                    "superseded_by": "",
                    "tags": ["py", "alpha"]
                }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        let r = get("abc123").unwrap();
        assert_eq!(r.body, "hello from python");
        assert_eq!(r.importance, 8);
        assert_eq!(r.tags, vec!["py".to_string(), "alpha".to_string()]);

        // Now mutate via Rust, read JSON back, confirm shape still has
        // the same top-level key.
        save("rust write", "rs", Some(5.0), vec![], None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            parsed.get("memories").is_some(),
            "top-level must be {{\"memories\": [...]}}"
        );
        let arr = parsed["memories"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn vector_path_matches_data_dir_layout() {
        let _env = TestEnv::new();
        let p = vector_path();
        assert!(p.ends_with("long_term.vec.bin"));
    }

    #[test]
    fn save_without_vector_skips_mirror_update() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // No vector → store stays empty, JSON has the record.
        save("no_vec", "", Some(5.0), vec![], None).unwrap();
        assert!(!vector_path().exists());
        let lst = list_records(true);
        assert_eq!(lst.len(), 1);
    }
}
