//! Telemetry log for RAG retrieval — misses, feedback, and per-chunk
//! retrieval counts. Rust port of `Core/harness/memory/miss_log.py`.
//!
//! Three JSONL/JSON files under `<data_dir>/miss_log/`:
//!
//! * `misses.jsonl` — append-only, one row per query (miss-flagged when
//!   retrieval came up short).
//! * `feedback.jsonl` — append-only, one row per ±1/0 user rating.
//! * `chunk_usage.json` — counter dict `{ chunk_id: { count, first_seen,
//!   last_used } }`.
//!
//! JSONL beats SQLite for the same reasons the Python module documents:
//! single-writer-mostly, no ad-hoc SQL, dep-light. The on-disk shapes
//! match Python byte-for-byte so the `rag_misses` / `rag_chunk_usage` /
//! `rag_feedback` tools can roll forward without touching disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::memory::common::{data_dir, ensure_dir};

const DIR_NAME: &str = "miss_log";
const MISSES_FILE: &str = "misses.jsonl";
const FEEDBACK_FILE: &str = "feedback.jsonl";
const CHUNKS_FILE: &str = "chunk_usage.json";

/// Process-wide write lock. Mirrors Python's module-level `threading.Lock`.
/// Cross-process writers must run their own coordination — this matches
/// the Python contract.
static IO_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Short, sortable id for a miss row. Mirrors Python's
/// `f"{int(_now() * 1000):x}-{secrets.token_hex(3)}"`.
fn new_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut buf);
    let ms = (now_epoch_secs() * 1000.0) as u64;
    format!("{ms:x}-{}", hex::encode(buf))
}

fn dir_path() -> PathBuf {
    data_dir().join(DIR_NAME)
}

fn misses_path() -> PathBuf {
    dir_path().join(MISSES_FILE)
}

fn feedback_path() -> PathBuf {
    dir_path().join(FEEDBACK_FILE)
}

fn chunks_path() -> PathBuf {
    dir_path().join(CHUNKS_FILE)
}

fn append_jsonl(path: &Path, row: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let line = serde_json::to_string(row).map_err(|e| std::io::Error::other(e.to_string()))?;
    let _guard = IO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            out.push(v);
        }
    }
    out
}

/// Counter row for `chunk_usage.json`. Top-level wrapper is a map keyed
/// by `chunk_id`; each entry carries `count`, `first_seen`, `last_used`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkUsageEntry {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub first_seen: f64,
    #[serde(default)]
    pub last_used: f64,
}

fn load_chunks() -> BTreeMap<String, ChunkUsageEntry> {
    let bytes = match std::fs::read(chunks_path()) {
        Ok(b) => b,
        Err(_) => return BTreeMap::new(),
    };
    serde_json::from_slice::<BTreeMap<String, ChunkUsageEntry>>(&bytes).unwrap_or_default()
}

fn save_chunks(doc: &BTreeMap<String, ChunkUsageEntry>) -> std::io::Result<()> {
    let path = chunks_path();
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(doc).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

// ─── Write APIs ─────────────────────────────────────────────────────────

/// Append a miss row. `context` is freeform — the rag_pipeline uses it
/// for retrieval scores, gate reason, candidate chunk ids, etc.
/// Returns the assigned id.
pub fn record_miss(query: &str, context: Option<Value>) -> String {
    let id = new_id();
    let mut row = Map::new();
    row.insert("id".into(), json!(id));
    row.insert("ts".into(), json!(now_epoch_secs()));
    row.insert("query".into(), json!(query));
    row.insert(
        "context".into(),
        match context {
            Some(Value::Object(o)) => Value::Object(o),
            Some(_) | None => Value::Object(Map::new()),
        },
    );
    let _ = append_jsonl(&misses_path(), &Value::Object(row));
    id
}

/// Log a RAG query — auto-called from `rag::search`. The context block
/// carries `hit_count` / `missed` so `list_misses` can filter at read
/// time without a second file. Returns the assigned query_id.
pub fn log_query(query: &str, workspace_id: &str, hits: &[Value], tier: Option<&str>) -> String {
    if query.trim().is_empty() {
        return String::new();
    }
    let chunk_ids: Vec<String> = hits
        .iter()
        .filter_map(|h| {
            h.get("id")
                .or_else(|| h.get("chunk_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let mut ctx = Map::new();
    ctx.insert("workspace_id".into(), json!(workspace_id));
    ctx.insert("hit_count".into(), json!(chunk_ids.len()));
    ctx.insert("missed".into(), json!(chunk_ids.is_empty()));
    let mut hit_ids = chunk_ids.clone();
    if hit_ids.len() > 20 {
        hit_ids.truncate(20);
    }
    ctx.insert("hit_ids".into(), json!(hit_ids));
    if let Some(t) = tier {
        ctx.insert("tier".into(), json!(t));
    }
    record_miss(query, Some(Value::Object(ctx)))
}

/// Append a feedback event for a prior query id. `rating` must be in
/// `{-1, 0, 1}`; values outside that range yield an `Err`. Disk IO
/// failure returns `Ok(false)` — matches Python's "soft-failure" path.
pub fn record_feedback(
    result_id: &str,
    rating: i32,
    comment: Option<&str>,
) -> Result<bool, String> {
    if !(-1..=1).contains(&rating) {
        return Err("rating must be -1, 0, or 1".to_owned());
    }
    let mut row = Map::new();
    row.insert("ts".into(), json!(now_epoch_secs()));
    row.insert("result_id".into(), json!(result_id));
    row.insert("rating".into(), json!(rating));
    if let Some(c) = comment {
        if !c.is_empty() {
            row.insert("comment".into(), json!(c));
        }
    }
    match append_jsonl(&feedback_path(), &Value::Object(row)) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Bump the per-chunk retrieval counter. Cheap enough to call once per
/// hit per query.
pub fn record_chunk_use(chunk_id: &str) {
    if chunk_id.is_empty() {
        return;
    }
    let now = now_epoch_secs();
    let _guard = IO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut doc = load_chunks();
    let entry = doc.entry(chunk_id.to_owned()).or_insert(ChunkUsageEntry {
        count: 0,
        first_seen: now,
        last_used: now,
    });
    entry.count += 1;
    entry.last_used = now;
    if entry.first_seen == 0.0 {
        entry.first_seen = now;
    }
    let _ = save_chunks(&doc);
}

// ─── Read APIs ──────────────────────────────────────────────────────────

/// Return recent miss rows, newest-first. Mirrors Python's
/// `list_misses(since, limit)` — only rows whose `context.missed` is
/// true (or `hit_count == 0`) make it through.
pub fn list_misses(since: Option<f64>, limit: usize) -> Vec<Value> {
    let mut rows: Vec<Value> = read_jsonl(&misses_path())
        .into_iter()
        .filter(is_miss_row)
        .collect();
    if let Some(cutoff) = since {
        rows.retain(|r| {
            r.get("ts")
                .and_then(Value::as_f64)
                .map(|t| t >= cutoff)
                .unwrap_or(false)
        });
    }
    rows.sort_by(|a, b| {
        let bt = b.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        let at = a.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        bt.partial_cmp(&at).unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);
    rows
}

fn is_miss_row(row: &Value) -> bool {
    let ctx = match row.get("context") {
        Some(Value::Object(m)) => m,
        _ => return true,
    };
    if let Some(missed) = ctx.get("missed").and_then(Value::as_bool) {
        return missed;
    }
    if let Some(hc) = ctx.get("hit_count").and_then(Value::as_i64) {
        return hc == 0;
    }
    true
}

/// Return the top-N most-retrieved chunks. Mirrors Python's
/// `chunk_usage(top)` — sorted by count descending.
pub fn chunk_usage(top: usize) -> Vec<Value> {
    let doc = load_chunks();
    let mut rows: Vec<Value> = doc
        .into_iter()
        .map(|(chunk_id, info)| {
            json!({
                "chunk_id": chunk_id,
                "count": info.count,
                "first_seen": info.first_seen,
                "last_used": info.last_used,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let bc = b.get("count").and_then(Value::as_i64).unwrap_or(0);
        let ac = a.get("count").and_then(Value::as_i64).unwrap_or(0);
        bc.cmp(&ac)
    });
    rows.truncate(top);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::rag::test_support::TestEnv;

    #[test]
    fn record_miss_writes_jsonl_row_and_returns_id() {
        let _env = TestEnv::new();
        let id = record_miss("why did it fail", Some(json!({"reason": "gate_fired"})));
        assert!(!id.is_empty());
        let rows = list_misses(None, 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["query"], "why did it fail");
        assert_eq!(rows[0]["context"]["reason"], "gate_fired");
    }

    #[test]
    fn log_query_records_hits_and_flags_miss_correctly() {
        let _env = TestEnv::new();
        let id_hit = log_query(
            "with hits",
            "ws-a",
            &[json!({"id": "c1"})],
            Some("episodic"),
        );
        let id_miss = log_query("no hits", "ws-a", &[], Some("core"));
        assert!(!id_hit.is_empty());
        assert!(!id_miss.is_empty());

        let misses = list_misses(None, 100);
        // Only the empty-hit query should surface.
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0]["query"], "no hits");
        assert_eq!(misses[0]["context"]["tier"], "core");
    }

    #[test]
    fn record_feedback_rejects_out_of_range() {
        let _env = TestEnv::new();
        assert!(record_feedback("qid", 2, None).is_err());
        assert!(record_feedback("qid", -2, None).is_err());
    }

    #[test]
    fn record_feedback_writes_row_when_valid() {
        let _env = TestEnv::new();
        let ok = record_feedback("qid-1", 1, Some("helpful")).unwrap();
        assert!(ok);
        let raw = std::fs::read_to_string(feedback_path()).unwrap();
        assert!(raw.contains("qid-1"));
        assert!(raw.contains("helpful"));
    }

    #[test]
    fn record_chunk_use_bumps_counter() {
        let _env = TestEnv::new();
        record_chunk_use("chunk-a");
        record_chunk_use("chunk-a");
        record_chunk_use("chunk-b");
        let usage = chunk_usage(10);
        // chunk-a count=2 should rank above chunk-b count=1.
        assert_eq!(usage[0]["chunk_id"], "chunk-a");
        assert_eq!(usage[0]["count"], 2);
        assert_eq!(usage[1]["chunk_id"], "chunk-b");
    }

    #[test]
    fn list_misses_sorts_newest_first() {
        let _env = TestEnv::new();
        // record_miss uses real time — two back-to-back calls should
        // still produce monotonic timestamps in practice; pin order by
        // asserting BOTH rows are present.
        let _id1 = record_miss("query 1", None);
        let _id2 = record_miss("query 2", None);
        let rows = list_misses(None, 100);
        assert_eq!(rows.len(), 2);
        // Sorted by ts desc — newest first.
        let t0 = rows[0]["ts"].as_f64().unwrap();
        let t1 = rows[1]["ts"].as_f64().unwrap();
        assert!(t0 >= t1);
    }

    #[test]
    fn list_misses_respects_limit() {
        let _env = TestEnv::new();
        for i in 0..5 {
            record_miss(&format!("q{i}"), None);
        }
        let rows = list_misses(None, 3);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn list_misses_returns_empty_when_no_file() {
        let _env = TestEnv::new();
        let rows = list_misses(None, 100);
        assert!(rows.is_empty());
    }

    #[test]
    fn new_id_is_unique_across_calls() {
        let ids: std::collections::HashSet<_> = (0..32).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 32);
    }
}
