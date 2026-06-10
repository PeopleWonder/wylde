//! Pure-Rust vector store backing the Phase 7.B long-term memory port.
//!
//! ## Why this exists
//!
//! The Python long-term layer mirrors its authoritative JSON records
//! into a LanceDB table for vector retrieval. The Rust port replaces
//! LanceDB with a small, dim-agnostic, single-file store. Wylde's
//! long-term tier is curated (importance-gated, never auto-ingested in
//! bulk) so the expected scale is low thousands of records at most —
//! linear cosine scan over a contiguous arena is faster than an HNSW
//! build+query at that scale, and trivially deterministic for tests.
//!
//! If scale ever balloons we swap `query_topk` for an HNSW index
//! behind the same trait surface.
//!
//! ## Public surface
//!
//! * [`VectorStore`] — owns a `Vec<Record>` keyed by id, with the
//!   embedding dim fixed at construction.
//! * [`VectorStore::insert`] — upsert (replace if id exists).
//! * [`VectorStore::delete`] — remove by id, returns whether anything
//!   matched.
//! * [`VectorStore::query_topk`] — cosine-similarity top-K. Empty store
//!   returns an empty list (not an error).
//! * [`VectorStore::persist`] / [`VectorStore::load`] — atomic
//!   round-trip via `<path>.tmp` + rename.
//!
//! ## On-disk format
//!
//! The store serialises with bincode under a versioned envelope:
//!
//! ```text
//! StoreOnDisk {
//!     version: u32,          // currently 1
//!     dim: u32,
//!     records: Vec<Record>,  // (id: String, vector: Vec<f32>)
//! }
//! ```
//!
//! Bincode default (little-endian, fixed-int) so the byte layout is
//! deterministic. Documented in
//! `memory/wylde_phase7b_long_term_shipped.md`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk envelope. Versioned so a format bump can read prior shapes.
#[derive(Debug, Serialize, Deserialize)]
struct StoreOnDisk {
    version: u32,
    dim: u32,
    records: Vec<Record>,
}

/// One row in the store: opaque id + dense embedding vector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub id: String,
    pub vector: Vec<f32>,
}

/// Score + id pair returned by [`VectorStore::query_topk`].
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    /// Cosine similarity in `[-1.0, 1.0]`. Pre-normalised on insert so
    /// the math reduces to a dot product.
    pub similarity: f32,
}

/// Errors raised by the store. Kept narrow — most operations just
/// return `Result<(), VectorStoreError>`.
#[derive(Debug, thiserror::Error)]
pub enum VectorStoreError {
    #[error("vector dim mismatch: store dim {expected}, supplied {actual}")]
    DimMismatch { expected: usize, actual: usize },
    #[error("vector is empty")]
    EmptyVector,
    #[error("on-disk store dim {on_disk} does not match expected {expected}")]
    LoadedDimMismatch { expected: usize, on_disk: usize },
    #[error("unsupported on-disk version {0}")]
    UnsupportedVersion(u32),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bincode error: {0}")]
    Bincode(#[from] bincode::Error),
}

/// Format version. Bump on any change that breaks reading prior bytes.
const FORMAT_VERSION: u32 = 1;

/// Pure-Rust vector store. Owns the records; dim is fixed at
/// construction.
#[derive(Debug, Clone)]
pub struct VectorStore {
    dim: usize,
    records: Vec<Record>,
}

impl VectorStore {
    /// New empty store with embedding dim `dim`.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            records: Vec::new(),
        }
    }

    /// Embedding dim. All vectors passed to [`insert`] / [`query_topk`]
    /// must have exactly this length.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Record count.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Upsert a record by id. If an existing record has the same id it
    /// is replaced; otherwise the record is appended. Vector is L2-
    /// normalised in place so [`query_topk`] reduces to a dot product.
    pub fn insert(
        &mut self,
        id: impl Into<String>,
        vector: Vec<f32>,
    ) -> Result<(), VectorStoreError> {
        if vector.is_empty() {
            return Err(VectorStoreError::EmptyVector);
        }
        if vector.len() != self.dim {
            return Err(VectorStoreError::DimMismatch {
                expected: self.dim,
                actual: vector.len(),
            });
        }
        let normed = l2_normalize(vector);
        let id = id.into();
        if let Some(slot) = self.records.iter_mut().find(|r| r.id == id) {
            slot.vector = normed;
        } else {
            self.records.push(Record { id, vector: normed });
        }
        Ok(())
    }

    /// Remove a record by id. Returns `true` if the id existed.
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.id != id);
        self.records.len() != before
    }

    /// Drop every record. Dim stays fixed.
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Top-K cosine-similarity hits for `query`. Empty store returns an
    /// empty Vec — not an error. Ties broken by id ascending so the
    /// ordering is deterministic across runs.
    ///
    /// `query` is L2-normalised in place before the dot-product scan;
    /// stored vectors are already normalised on insert so the dot
    /// product equals the cosine.
    pub fn query_topk(&self, query: Vec<f32>, k: usize) -> Result<Vec<Hit>, VectorStoreError> {
        if query.is_empty() {
            return Err(VectorStoreError::EmptyVector);
        }
        if query.len() != self.dim {
            return Err(VectorStoreError::DimMismatch {
                expected: self.dim,
                actual: query.len(),
            });
        }
        if k == 0 || self.records.is_empty() {
            return Ok(Vec::new());
        }
        let q = l2_normalize(query);
        let mut hits: Vec<Hit> = self
            .records
            .iter()
            .map(|r| Hit {
                id: r.id.clone(),
                similarity: dot(&q, &r.vector),
            })
            .collect();
        hits.sort_by(|a, b| {
            // Higher similarity first; tie-break by id asc for determinism.
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);
        Ok(hits)
    }

    /// Lookup a record by id. Returns the stored (L2-normalised) vector.
    pub fn get(&self, id: &str) -> Option<&Record> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Iterate all records in insertion order. Useful for reindex
    /// passes that rebuild the store from a JSON authoritative list.
    pub fn iter(&self) -> impl Iterator<Item = &Record> {
        self.records.iter()
    }

    /// Atomically persist to `path`. Writes `<path>.tmp` then renames.
    /// Parent directory is NOT created — caller is responsible.
    pub fn persist(&self, path: &Path) -> Result<(), VectorStoreError> {
        let envelope = StoreOnDisk {
            version: FORMAT_VERSION,
            dim: self.dim as u32,
            records: self.records.clone(),
        };
        let bytes = bincode::serialize(&envelope)?;
        let tmp = with_tmp_suffix(path);
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a previously-persisted store. Errors if the on-disk dim
    /// doesn't match `expected_dim` — caller decides what to do (most
    /// likely: rebuild via reindex).
    pub fn load(path: &Path, expected_dim: usize) -> Result<Self, VectorStoreError> {
        let bytes = std::fs::read(path)?;
        let envelope: StoreOnDisk = bincode::deserialize(&bytes)?;
        if envelope.version != FORMAT_VERSION {
            return Err(VectorStoreError::UnsupportedVersion(envelope.version));
        }
        if envelope.dim as usize != expected_dim {
            return Err(VectorStoreError::LoadedDimMismatch {
                expected: expected_dim,
                on_disk: envelope.dim as usize,
            });
        }
        Ok(Self {
            dim: expected_dim,
            records: envelope.records,
        })
    }

    /// Load if the file exists, else return a fresh empty store. The
    /// `expected_dim` arg drives the empty-store dim too, so a brand
    /// new long-term layer comes up at the configured embedding width.
    ///
    /// If the file exists but its dim doesn't match, returns a fresh
    /// empty store and logs — matches Python's "JSON unreadable, treat
    /// as empty" recovery. The JSON authoritative list is canonical;
    /// the vector mirror is rebuildable.
    pub fn load_or_empty(path: &Path, expected_dim: usize) -> Self {
        if !path.exists() {
            return Self::new(expected_dim);
        }
        match Self::load(path, expected_dim) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "vector_store: load failed for {} ({}), starting empty",
                    path.display(),
                    e
                );
                Self::new(expected_dim)
            }
        }
    }
}

fn with_tmp_suffix(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        let inv = 1.0 / norm;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn new_store_is_empty_and_has_configured_dim() {
        let s = VectorStore::new(128);
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.dim(), 128);
    }

    #[test]
    fn insert_normalises_vector_in_place() {
        let mut s = VectorStore::new(3);
        s.insert("a", vec![3.0, 0.0, 0.0]).unwrap();
        let r = s.get("a").unwrap();
        // Should be (1, 0, 0) after L2 normalisation.
        assert!(approx(r.vector[0], 1.0));
        assert!(approx(r.vector[1], 0.0));
        assert!(approx(r.vector[2], 0.0));
    }

    #[test]
    fn insert_with_wrong_dim_returns_dim_mismatch() {
        let mut s = VectorStore::new(3);
        let err = s.insert("a", vec![1.0, 2.0]).unwrap_err();
        match err {
            VectorStoreError::DimMismatch { expected, actual } => {
                assert_eq!(expected, 3);
                assert_eq!(actual, 2);
            }
            other => panic!("expected DimMismatch, got {other:?}"),
        }
    }

    #[test]
    fn insert_with_empty_vector_returns_empty_error() {
        let mut s = VectorStore::new(3);
        let err = s.insert("a", vec![]).unwrap_err();
        matches!(err, VectorStoreError::EmptyVector);
    }

    #[test]
    fn insert_with_same_id_replaces_existing_record() {
        let mut s = VectorStore::new(3);
        s.insert("a", vec![1.0, 0.0, 0.0]).unwrap();
        s.insert("a", vec![0.0, 1.0, 0.0]).unwrap();
        assert_eq!(s.len(), 1);
        let r = s.get("a").unwrap();
        assert!(approx(r.vector[1], 1.0));
    }

    #[test]
    fn delete_returns_true_for_known_id_false_for_unknown() {
        let mut s = VectorStore::new(3);
        s.insert("a", vec![1.0, 0.0, 0.0]).unwrap();
        assert!(s.delete("a"));
        assert!(!s.delete("a"));
        assert!(s.is_empty());
    }

    #[test]
    fn query_topk_on_empty_store_returns_empty_list_not_error() {
        let s = VectorStore::new(3);
        let hits = s.query_topk(vec![1.0, 0.0, 0.0], 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn query_topk_with_k_zero_returns_empty_list() {
        let mut s = VectorStore::new(3);
        s.insert("a", vec![1.0, 0.0, 0.0]).unwrap();
        let hits = s.query_topk(vec![1.0, 0.0, 0.0], 0).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn query_topk_returns_records_sorted_by_cosine_descending() {
        // Three vectors in 3D: e1, e2, and a vector close to e1.
        let mut s = VectorStore::new(3);
        s.insert("e1", vec![1.0, 0.0, 0.0]).unwrap();
        s.insert("e2", vec![0.0, 1.0, 0.0]).unwrap();
        s.insert("near_e1", vec![0.9, 0.1, 0.0]).unwrap();

        let hits = s.query_topk(vec![1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3);
        // Best match is e1 (similarity 1.0), then near_e1, then e2.
        assert_eq!(hits[0].id, "e1");
        assert!(approx(hits[0].similarity, 1.0));
        assert_eq!(hits[1].id, "near_e1");
        assert!(hits[1].similarity > hits[2].similarity);
        assert_eq!(hits[2].id, "e2");
        assert!(approx(hits[2].similarity, 0.0));
    }

    #[test]
    fn query_topk_breaks_ties_by_id_ascending() {
        let mut s = VectorStore::new(3);
        // Two identical vectors — tie on similarity, id determines order.
        s.insert("b", vec![1.0, 0.0, 0.0]).unwrap();
        s.insert("a", vec![1.0, 0.0, 0.0]).unwrap();
        let hits = s.query_topk(vec![1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits[0].id, "a");
        assert_eq!(hits[1].id, "b");
    }

    #[test]
    fn query_topk_dim_mismatch_returns_error() {
        let s = VectorStore::new(3);
        let err = s.query_topk(vec![1.0, 0.0], 5).unwrap_err();
        matches!(err, VectorStoreError::DimMismatch { .. });
    }

    #[test]
    fn query_topk_caps_at_record_count_when_k_exceeds_len() {
        let mut s = VectorStore::new(3);
        s.insert("a", vec![1.0, 0.0, 0.0]).unwrap();
        s.insert("b", vec![0.0, 1.0, 0.0]).unwrap();
        let hits = s.query_topk(vec![1.0, 0.0, 0.0], 100).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn persist_and_load_round_trip_preserves_records() {
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");
        let mut s = VectorStore::new(4);
        s.insert("a", vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        s.insert("b", vec![5.0, 6.0, 7.0, 8.0]).unwrap();
        s.persist(&path).unwrap();

        let back = VectorStore::load(&path, 4).unwrap();
        assert_eq!(back.dim(), 4);
        assert_eq!(back.len(), 2);
        // Insertion order preserved across persist/load.
        let mut it = back.iter();
        assert_eq!(it.next().unwrap().id, "a");
        assert_eq!(it.next().unwrap().id, "b");
        // Vectors are byte-identical (already normalised pre-write).
        assert_eq!(s.get("a").unwrap().vector, back.get("a").unwrap().vector);
    }

    #[test]
    fn load_errors_when_disk_dim_does_not_match_expected() {
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");
        let mut s = VectorStore::new(4);
        s.insert("a", vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        s.persist(&path).unwrap();

        let err = VectorStore::load(&path, 8).unwrap_err();
        match err {
            VectorStoreError::LoadedDimMismatch { expected, on_disk } => {
                assert_eq!(expected, 8);
                assert_eq!(on_disk, 4);
            }
            other => panic!("expected LoadedDimMismatch, got {other:?}"),
        }
    }

    #[test]
    fn load_or_empty_returns_empty_when_path_missing() {
        let td = tempdir().unwrap();
        let path = td.path().join("missing.bin");
        let s = VectorStore::load_or_empty(&path, 4);
        assert!(s.is_empty());
        assert_eq!(s.dim(), 4);
    }

    #[test]
    fn load_or_empty_returns_empty_when_dim_mismatch_on_disk() {
        // JSON authoritative; if vector mirror is wrong-dim we silently
        // start empty and the next save() will rebuild it.
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");
        let mut original = VectorStore::new(4);
        original.insert("a", vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        original.persist(&path).unwrap();

        let fresh = VectorStore::load_or_empty(&path, 8);
        assert!(fresh.is_empty());
        assert_eq!(fresh.dim(), 8);
    }

    #[test]
    fn persist_uses_atomic_rename_via_tmp_suffix() {
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");
        let mut s = VectorStore::new(3);
        s.insert("a", vec![1.0, 0.0, 0.0]).unwrap();
        s.persist(&path).unwrap();
        // tmp file must not be left behind.
        let tmp = with_tmp_suffix(&path);
        assert!(!tmp.exists(), "tmp file left behind: {tmp:?}");
        assert!(path.exists());
    }

    #[test]
    fn fixed_seed_topk_is_deterministic_across_runs() {
        // Reproducible synthetic dataset — embed N seeded vectors,
        // assert top-1 hit for a query is always the closest by id.
        let mut s = VectorStore::new(4);
        let dataset = [
            ("alpha", [1.0_f32, 0.0, 0.0, 0.0]),
            ("beta", [0.0, 1.0, 0.0, 0.0]),
            ("gamma", [0.0, 0.0, 1.0, 0.0]),
            ("delta", [0.0, 0.0, 0.0, 1.0]),
            ("near_alpha", [0.95, 0.05, 0.0, 0.0]),
        ];
        for (id, v) in &dataset {
            s.insert(*id, v.to_vec()).unwrap();
        }
        let hits = s.query_topk(vec![1.0, 0.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits[0].id, "alpha");
        assert_eq!(hits[1].id, "near_alpha");
    }
}
