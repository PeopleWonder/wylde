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
use crate::memory::common::{data_dir, ensure_dir};
use crate::memory::long_term::{combined_score, normalize_importance, DEFAULT_DECAY_DAYS};

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

/// Recursively remove the durable workspace memory folder. Invoked on
/// explicit user delete of a workspace — MRU eviction must NOT call
/// this. Returns `true` if a folder was removed. Mirrors Python's
/// `delete_memory_dir`.
pub fn delete_memory_dir(workspace_id: &str) -> bool {
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
            tracing::warn!(
                "workspace_memory: {} JSON unreadable: {}",
                workspace_id,
                e
            );
            return Vec::new();
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "workspace_memory: {} JSON unreadable: {}",
                workspace_id,
                e
            );
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
    let remaining: Vec<WorkspaceMemory> = records
        .iter()
        .filter(|r| !ids.contains(r.id.as_str()))
        .cloned()
        .collect();
    if let Err(e) = save_all(workspace_id, &remaining) {
        tracing::warn!("workspace_memory: save_all failed during delete: {}", e);
        return false;
    }
    true
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
        let r = save_new("ws1", "  hello world  ", "chat", Some(7.0), vec!["e1".into()])
            .unwrap();
        assert_eq!(r.id.len(), 16);
        assert!(r.id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
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
            save_new("ws1", &format!("common token row{i}"), "", Some(5.0), vec![]).unwrap();
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
