//! Tiered semantic memory store.
//!
//! Sister of `crate::memory::long_term`, dedicated to the
//! core/episodic/semantic/procedural tiers of the in-process RAG layer.
//! Same two-half pattern: a JSON sidecar of authoritative records, and a
//! [`crate::memory::vector::VectorStore`] mirror for top-K cosine search.
//!
//! ## Why a second store?
//!
//! Long-term records carry importance / decay scoring and supersession
//! chains; tier records carry only `(id, content, tier, similarity-
//! score, created_at, session_id, source_path)`. Folding both into one
//! store would either pollute the long_term tests with tier-only fields
//! or force long_term to grow optional bag-fields. Two small stores stay
//! independently testable and the on-disk shape stays close to Python's
//! `vector_store.py` row shape.
//!
//! ## On-disk layout
//!
//! ```text
//! <data_dir>/rag_tiers.json      ← authoritative records, JSON
//! <data_dir>/rag_tiers.vec.bin   ← bincode vector mirror
//! ```
//!
//! The vector mirror is rebuilt from JSON on dim mismatch or corruption
//! — same `load_or_empty` recovery that 7.B-1 documented.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::memory::common::{data_dir, embed_dim, ensure_dir};
use crate::memory::vector::{VectorStore, VectorStoreError};

const JSON_NAME: &str = "rag_tiers.json";
const VEC_NAME: &str = "rag_tiers.vec.bin";

/// One row in the tiered store. Field shapes mirror `vector_store.py`'s
/// rows so a future parity test can compare JSON output without any
/// translation layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierRecord {
    pub id: String,
    pub content: String,
    /// One of the tier constants in [`crate::memory::rag::tiers`].
    pub memory_type: String,
    /// Tier-side importance score. Matches Python's `score` field —
    /// distinct from vector cosine similarity (which is computed at
    /// query time and lives on [`crate::memory::rag::search::Hit`]).
    pub score: f32,
    pub created_at: f64,
    pub session_id: String,
    pub source_path: String,
}

impl TierRecord {
    pub fn new(
        id: impl Into<String>,
        content: impl Into<String>,
        memory_type: impl Into<String>,
        score: f32,
        session_id: impl Into<String>,
        source_path: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            memory_type: memory_type.into(),
            score,
            created_at: now_epoch_secs(),
            session_id: session_id.into(),
            source_path: source_path.into(),
        }
    }
}

fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Tiered store. Owns the JSON record list + the VectorStore mirror.
/// One instance per data dir; pure-in-memory between `save` calls.
#[derive(Debug)]
pub struct TieredStore {
    records: Vec<TierRecord>,
    vectors: VectorStore,
    json_path: PathBuf,
    vec_path: PathBuf,
}

impl TieredStore {
    /// Build a store from the current `WYLDE_DATA_DIR`. The vector dim
    /// is read from the env (`WYLDE_EMBED_DIM`); a fresh empty store is
    /// returned if either file is missing.
    pub fn open() -> Self {
        Self::open_at(&data_dir(), embed_dim())
    }

    /// Build a store from an explicit directory + dim. Used by tests and
    /// by callers that want to mount a non-default data dir.
    pub fn open_at(dir: &Path, dim: usize) -> Self {
        let json_path = dir.join(JSON_NAME);
        let vec_path = dir.join(VEC_NAME);
        let records = load_records(&json_path).unwrap_or_default();
        let vectors = VectorStore::load_or_empty(&vec_path, dim);
        Self {
            records,
            vectors,
            json_path,
            vec_path,
        }
    }

    pub fn json_path(&self) -> &Path {
        &self.json_path
    }

    pub fn vec_path(&self) -> &Path {
        &self.vec_path
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &TierRecord> {
        self.records.iter()
    }

    /// Total row count — matches Python's `vector_store.count_rows`.
    pub fn count_rows(&self) -> usize {
        self.records.len()
    }

    /// Insert (or replace) a record. The vector mirror is updated when
    /// `vector` is `Some`, left untouched otherwise. The latter case
    /// matches the long_term contract: vectors are optional at write
    /// time and the next reindex pass restores parity.
    pub fn insert(
        &mut self,
        record: TierRecord,
        vector: Option<Vec<f32>>,
    ) -> Result<(), VectorStoreError> {
        if let Some(slot) = self.records.iter_mut().find(|r| r.id == record.id) {
            *slot = record.clone();
        } else {
            self.records.push(record.clone());
        }
        if let Some(v) = vector {
            self.vectors.insert(record.id.clone(), v)?;
        }
        Ok(())
    }

    /// Remove records by id. Returns the count actually removed (mirror
    /// of Python's `delete_rows`).
    pub fn delete_rows(&mut self, ids: &[String]) -> usize {
        let before = self.records.len();
        self.records.retain(|r| !ids.iter().any(|x| x == &r.id));
        let removed = before - self.records.len();
        for id in ids {
            self.vectors.delete(id);
        }
        removed
    }

    /// Filter rows by tier / score floor / limit. Mirrors Python's
    /// `vector_store.list_rows(memory_type, score_lt, limit)` — the
    /// score predicate is "strictly less than" so a `score_lt = 0.5`
    /// retains rows with score < 0.5.
    pub fn list_rows(
        &self,
        memory_type: Option<&str>,
        score_lt: Option<f32>,
        limit: usize,
    ) -> Vec<TierRecord> {
        let mut out: Vec<TierRecord> = self
            .records
            .iter()
            .filter(|r| match memory_type {
                Some(t) => r.memory_type == t,
                None => true,
            })
            .filter(|r| match score_lt {
                Some(thr) => r.score < thr,
                None => true,
            })
            .cloned()
            .collect();
        if out.len() > limit {
            out.truncate(limit);
        }
        out
    }

    /// Vector cosine top-K with an optional tier filter. Mirrors
    /// Python's `vector_store.search_vectors(qvec, memory_type, limit)`.
    /// Returns rich rows (record + similarity) — the consumer is
    /// `rag::search`.
    pub fn search_vectors(
        &self,
        query_vector: Vec<f32>,
        memory_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TierHit>, VectorStoreError> {
        // Over-fetch before the tier filter so the top-K post-filter
        // still has `limit` results when the filter is strict.
        let fetch_k = match memory_type {
            Some(_) => self.records.len().max(limit),
            None => limit,
        };
        let raw = self.vectors.query_topk(query_vector, fetch_k)?;
        let mut out: Vec<TierHit> = Vec::with_capacity(raw.len());
        for hit in raw {
            let Some(record) = self.records.iter().find(|r| r.id == hit.id).cloned() else {
                // Mirror is ahead of the JSON; skip — the next save will
                // bring them back in sync.
                continue;
            };
            if let Some(t) = memory_type {
                if record.memory_type != t {
                    continue;
                }
            }
            out.push(TierHit {
                record,
                similarity: hit.similarity,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Persist both halves to disk under the configured paths. Atomic on
    /// the vector side via the bincode store's `.tmp + rename`; the JSON
    /// side uses the same pattern via [`write_json_atomically`].
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.json_path.parent() {
            ensure_dir(parent)?;
        }
        write_json_atomically(&self.json_path, &self.records)?;
        self.vectors
            .persist(&self.vec_path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Borrow the inner vector mirror — useful for tests pinning the
    /// dim-agnostic store invariant.
    #[cfg(test)]
    pub(crate) fn vectors(&self) -> &VectorStore {
        &self.vectors
    }
}

/// One row plus the cosine similarity that surfaced it.
#[derive(Debug, Clone, PartialEq)]
pub struct TierHit {
    pub record: TierRecord,
    pub similarity: f32,
}

fn load_records(path: &Path) -> Option<Vec<TierRecord>> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Vec<TierRecord>>(&bytes).ok()
}

fn write_json_atomically(path: &Path, records: &[TierRecord]) -> std::io::Result<()> {
    let bytes =
        serde_json::to_vec_pretty(records).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn new_store(td: &TempDir) -> TieredStore {
        TieredStore::open_at(td.path(), 4)
    }

    fn rec(id: &str, tier: &str, score: f32) -> TierRecord {
        TierRecord::new(id, format!("body of {id}"), tier, score, "", "")
    }

    #[test]
    fn open_empty_dir_yields_empty_store() {
        let td = TempDir::new().unwrap();
        let s = new_store(&td);
        assert!(s.is_empty());
        assert_eq!(s.count_rows(), 0);
    }

    #[test]
    fn insert_with_vector_round_trips_through_save_load() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        s.insert(rec("a", "episodic", 0.5), Some(vec![1.0, 0.0, 0.0, 0.0]))
            .unwrap();
        s.insert(rec("b", "core", 1.0), Some(vec![0.0, 1.0, 0.0, 0.0]))
            .unwrap();
        s.save().unwrap();

        let back = TieredStore::open_at(td.path(), 4);
        assert_eq!(back.count_rows(), 2);
        let hits = back
            .search_vectors(vec![1.0, 0.0, 0.0, 0.0], None, 2)
            .unwrap();
        assert_eq!(hits[0].record.id, "a");
    }

    #[test]
    fn insert_without_vector_persists_json_only() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        s.insert(rec("a", "episodic", 0.5), None).unwrap();
        assert_eq!(s.count_rows(), 1);
        // Vector mirror untouched.
        assert!(s.vectors().is_empty());
    }

    #[test]
    fn insert_with_same_id_replaces_record() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        s.insert(rec("a", "episodic", 0.5), None).unwrap();
        s.insert(rec("a", "core", 1.0), None).unwrap();
        assert_eq!(s.count_rows(), 1);
        assert_eq!(s.iter().next().unwrap().memory_type, "core");
    }

    #[test]
    fn search_vectors_filters_by_tier() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        // Two near-identical embeddings; only one matches the tier filter.
        s.insert(rec("ep", "episodic", 0.5), Some(vec![1.0, 0.0, 0.0, 0.0]))
            .unwrap();
        s.insert(rec("co", "core", 0.5), Some(vec![0.9, 0.1, 0.0, 0.0]))
            .unwrap();

        let hits = s
            .search_vectors(vec![1.0, 0.0, 0.0, 0.0], Some("core"), 5)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.id, "co");
    }

    #[test]
    fn search_vectors_returns_empty_when_store_empty() {
        let td = TempDir::new().unwrap();
        let s = new_store(&td);
        let hits = s.search_vectors(vec![1.0, 0.0, 0.0, 0.0], None, 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn delete_rows_removes_from_both_halves() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        s.insert(rec("a", "episodic", 0.5), Some(vec![1.0, 0.0, 0.0, 0.0]))
            .unwrap();
        s.insert(rec("b", "core", 1.0), Some(vec![0.0, 1.0, 0.0, 0.0]))
            .unwrap();
        let removed = s.delete_rows(&["a".to_owned()]);
        assert_eq!(removed, 1);
        assert_eq!(s.count_rows(), 1);
        let hits = s.search_vectors(vec![1.0, 0.0, 0.0, 0.0], None, 5).unwrap();
        // Vector mirror cleaned up too — "a" no longer surfaces.
        assert!(hits.iter().all(|h| h.record.id != "a"));
    }

    #[test]
    fn list_rows_filters_by_tier_score_and_limit() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        s.insert(rec("a", "episodic", 0.4), None).unwrap();
        s.insert(rec("b", "episodic", 0.7), None).unwrap();
        s.insert(rec("c", "core", 0.3), None).unwrap();

        let by_tier = s.list_rows(Some("episodic"), None, 100);
        assert_eq!(by_tier.len(), 2);

        let weak = s.list_rows(None, Some(0.5), 100);
        let weak_ids: Vec<&str> = weak.iter().map(|r| r.id.as_str()).collect();
        assert!(weak_ids.contains(&"a"));
        assert!(weak_ids.contains(&"c"));
        assert!(!weak_ids.contains(&"b"));

        let limited = s.list_rows(None, None, 2);
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn save_writes_authoritative_json_and_vector_mirror() {
        let td = TempDir::new().unwrap();
        let mut s = new_store(&td);
        s.insert(rec("a", "core", 1.0), Some(vec![1.0, 0.0, 0.0, 0.0]))
            .unwrap();
        s.save().unwrap();
        assert!(td.path().join(JSON_NAME).exists());
        assert!(td.path().join(VEC_NAME).exists());
    }
}
