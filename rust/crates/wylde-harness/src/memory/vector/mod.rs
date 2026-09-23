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
//!     version: u32,          // currently 3
//!     dim: u32,
//!     embed_model: String,   // v3 (#136)
//!     records: Vec<Record>,  // (id: String, q: Vec<i8>, scale: f32)
//! }
//! ```
//!
//! **Version 3 (#136):** the envelope stamps the embedder that produced
//! the vectors, so a model swap at the same width is detected instead of
//! silently mixing incomparable vectors. An incompatible mirror (wrong
//! width or wrong model) is moved aside to `<path>.incompatible` rather
//! than left to be overwritten by the next write, and
//! [`rebuild`] regenerates it from the tier's authoritative JSON.
//! Version-2 files (no stamp) load transparently and adopt the current
//! model on the next persist.
//!
//! **Version 2 (improvement plan B13):** vectors are stored int8
//! scalar-quantised with a per-vector scale — `real[i] ≈ q[i] * scale` —
//! for ~4× smaller files and a faster scan; cosine on dequantised values
//! loses ~nothing at this corpus scale (the Matryoshka truncate-
//! renormalise seam in `embeddings.rs` already trades embedding
//! precision deliberately). Version-1 files (raw `Vec<f32>`) are read
//! transparently and quantised on load; the next persist writes v2.
//!
//! Bincode default (little-endian, fixed-int) so the byte layout is
//! deterministic. Documented in
//! `memory/wylde_phase7b_long_term_shipped.md`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod rebuild;

pub use rebuild::{rebuild, RebuildError, RebuildReport};

/// On-disk envelope. Versioned so a format bump can read prior shapes.
///
/// **Version 3 (#136)** adds `embed_model`. Without it a mirror carried no
/// record of *which embedder produced its vectors*, so swapping
/// `WYLDE_EMBED_MODEL` at the same width left every prior vector in place,
/// silently incomparable with the new ones — search quality degrading with no
/// signal anywhere. The workspaces RAG index already stamped its model in
/// `manifest.rs` and rebuilt on mismatch; the memory tiers did not, and that
/// asymmetry was the bug.
#[derive(Debug, Serialize, Deserialize)]
struct StoreOnDisk {
    version: u32,
    dim: u32,
    /// The embedder that produced these vectors (`embed_model()` at write
    /// time). A mismatch on load means the mirror is incomparable, not merely
    /// stale.
    embed_model: String,
    records: Vec<Record>,
}

/// The v2 envelope — model-less. Read transparently so existing mirrors
/// migrate instead of being discarded.
#[derive(Debug, Serialize, Deserialize)]
struct StoreOnDiskV2 {
    #[allow(dead_code)] // peeked before deserialisation; kept for shape parity
    version: u32,
    dim: u32,
    records: Vec<Record>,
}

/// The pre-B13 on-disk shapes, kept readable for transparent migration
/// (Serialize retained so the migration test can mint v1 bytes).
#[derive(Debug, Serialize, Deserialize)]
struct RecordV1 {
    id: String,
    vector: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreOnDiskV1 {
    #[allow(dead_code)] // peeked before deserialisation; kept for shape parity
    version: u32,
    dim: u32,
    records: Vec<RecordV1>,
}

/// One row in the store: opaque id + int8-quantised embedding (B13).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub id: String,
    /// int8 scalar-quantised, L2-normalised embedding:
    /// `real[i] ≈ q[i] as f32 * scale`.
    pub q: Vec<i8>,
    /// Per-vector dequantisation scale (`max |component| / 127`).
    pub scale: f32,
}

impl Record {
    /// The dequantised (approximately L2-normalised) vector.
    pub fn dequantized(&self) -> Vec<f32> {
        self.q.iter().map(|&x| x as f32 * self.scale).collect()
    }
}

/// Quantise an (already L2-normalised) vector to int8 + scale.
fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    let max_abs = v.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
    if max_abs == 0.0 {
        return (vec![0; v.len()], 0.0);
    }
    let scale = max_abs / 127.0;
    let q = v
        .iter()
        .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8)
        .collect();
    (q, scale)
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
    #[error("on-disk store was written by embedder {on_disk:?}, expected {expected:?}")]
    LoadedModelMismatch { expected: String, on_disk: String },
    #[error("unsupported on-disk version {0}")]
    UnsupportedVersion(u32),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bincode error: {0}")]
    Bincode(#[from] bincode::Error),
}

/// Format version. Bump on any change that breaks reading prior bytes.
/// v3 = `embed_model` stamped in the envelope (#136); v2 = int8 quantised
/// records (B13); v1 (raw f32) still loads.
const FORMAT_VERSION: u32 = 3;

/// Pure-Rust vector store. Owns the records; dim is fixed at
/// construction.
#[derive(Debug, Clone)]
pub struct VectorStore {
    dim: usize,
    /// Embedder that produced these vectors; stamped on persist (#136).
    embed_model: String,
    records: Vec<Record>,
}

impl VectorStore {
    /// New empty store with embedding dim `dim`, stamped with the currently
    /// configured embedder.
    pub fn new(dim: usize) -> Self {
        Self::new_with_model(dim, crate::memory::common::embed_model())
    }

    /// New empty store with an explicit embedder stamp. Test seam, and the
    /// entry point a rebuild uses when it wants to pin the model it embedded
    /// under rather than re-resolving it mid-pass.
    pub fn new_with_model(dim: usize, embed_model: String) -> Self {
        Self {
            dim,
            embed_model,
            records: Vec::new(),
        }
    }

    /// The embedder this store's vectors were produced by.
    pub fn embed_model(&self) -> &str {
        &self.embed_model
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
        let (q, scale) = quantize(&normed);
        let id = id.into();
        if let Some(slot) = self.records.iter_mut().find(|r| r.id == id) {
            slot.q = q;
            slot.scale = scale;
        } else {
            self.records.push(Record { id, q, scale });
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
        let qv = l2_normalize(query);
        let mut hits: Vec<Hit> = self
            .records
            .iter()
            .map(|r| Hit {
                id: r.id.clone(),
                similarity: dot_dequant(&qv, r),
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
            embed_model: self.embed_model.clone(),
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
    ///
    /// Version-1 files (raw f32 vectors) load transparently: each record
    /// is quantised in memory (B13) and the next [`Self::persist`] writes
    /// the v2 shape — a lazy, lossless-enough migration.
    pub fn load(path: &Path, expected_dim: usize) -> Result<Self, VectorStoreError> {
        Self::load_expecting(path, expected_dim, &crate::memory::common::embed_model())
    }

    /// [`Self::load`] with the expected embedder passed explicitly rather than
    /// resolved from config. Lets a caller (and the tests) reason about a
    /// specific model without mutating process-wide state.
    pub fn load_expecting(
        path: &Path,
        expected_dim: usize,
        expected_model: &str,
    ) -> Result<Self, VectorStoreError> {
        let bytes = std::fs::read(path)?;
        // Bincode is not self-describing: peek the version (first u32,
        // little-endian fixed-int) before choosing the record shape.
        let version = bytes
            .get(0..4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .unwrap_or(0);
        let (dim, on_disk_model, records): (u32, Option<String>, Vec<Record>) = match version {
            FORMAT_VERSION => {
                let envelope: StoreOnDisk = bincode::deserialize(&bytes)?;
                (envelope.dim, Some(envelope.embed_model), envelope.records)
            }
            2 => {
                // Pre-#136: no model stamp. We cannot know which embedder
                // produced these vectors, so we do NOT guess — an unstamped
                // mirror is accepted as-is and re-stamped with the current
                // model on the next persist. This is the one unavoidable
                // blind spot; it exists once, for stores written before the
                // field existed.
                let envelope: StoreOnDiskV2 = bincode::deserialize(&bytes)?;
                (envelope.dim, None, envelope.records)
            }
            1 => {
                let envelope: StoreOnDiskV1 = bincode::deserialize(&bytes)?;
                let migrated = envelope
                    .records
                    .into_iter()
                    .map(|r| {
                        // v1 vectors were L2-normalised on insert already.
                        let (q, scale) = quantize(&r.vector);
                        Record { id: r.id, q, scale }
                    })
                    .collect();
                (envelope.dim, None, migrated)
            }
            other => return Err(VectorStoreError::UnsupportedVersion(other)),
        };
        if dim as usize != expected_dim {
            return Err(VectorStoreError::LoadedDimMismatch {
                expected: expected_dim,
                on_disk: dim as usize,
            });
        }
        if let Some(found) = &on_disk_model {
            if found != expected_model {
                return Err(VectorStoreError::LoadedModelMismatch {
                    expected: expected_model.to_owned(),
                    on_disk: found.clone(),
                });
            }
        }
        Ok(Self {
            dim: expected_dim,
            embed_model: expected_model.to_owned(),
            records,
        })
    }

    /// Load if the file exists, else return a fresh empty store. The
    /// `expected_dim` arg drives the empty-store dim too, so a brand
    /// new long-term layer comes up at the configured embedding width.
    ///
    /// If the file exists but is incompatible — a different embedding width
    /// or a different embedder (#136) — the existing file is **preserved**,
    /// moved aside to `<path>.incompatible`, and a fresh empty store is
    /// returned for the rebuild to fill.
    ///
    /// # Why it moves the file instead of ignoring it
    ///
    /// This used to return an empty store and leave the old file in place,
    /// which sounds harmless but wasn't: the very next `vector_upsert`
    /// persists the empty-plus-one store straight over it, so every stored
    /// vector was destroyed by the first write after a dim change. One
    /// `warn!` in a log nobody reads was the only trace.
    ///
    /// The destruction was justified in this module's own comments by the
    /// claim that "the vector mirror is rebuildable" via a `reindex` — a
    /// function that did not exist. It exists now
    /// ([`crate::memory::vector::rebuild`] and the `*.reindex` verbs), so the
    /// mirror genuinely is rebuildable from the authoritative JSON; but a
    /// rebuild needs a working embedder, so the old vectors are kept aside
    /// rather than dropped on the floor in the meantime.
    ///
    /// A pre-existing `.incompatible` sidecar is overwritten: it is itself a
    /// derived artifact, and keeping an unbounded chain of them would trade
    /// one disk-growth bug for another.
    pub fn load_or_empty(path: &Path, expected_dim: usize) -> Self {
        Self::load_or_empty_expecting(path, expected_dim, &crate::memory::common::embed_model())
    }

    /// [`Self::load_or_empty`] with the expected embedder passed explicitly.
    pub fn load_or_empty_expecting(path: &Path, expected_dim: usize, expected_model: &str) -> Self {
        if !path.exists() {
            return Self::new_with_model(expected_dim, expected_model.to_owned());
        }
        match Self::load_expecting(path, expected_dim, expected_model) {
            Ok(s) => s,
            Err(e) => {
                let aside = quarantine_path(path);
                match std::fs::rename(path, &aside) {
                    Ok(()) => tracing::error!(
                        "vector_store: {} is incompatible ({}); moved to {} and starting \
                         empty — run the tier's reindex verb to rebuild the mirror from \
                         the authoritative JSON",
                        path.display(),
                        e,
                        aside.display(),
                    ),
                    Err(rename_err) => tracing::error!(
                        "vector_store: {} is incompatible ({}) and could NOT be preserved \
                         ({}); starting empty — the next write will overwrite it",
                        path.display(),
                        e,
                        rename_err,
                    ),
                }
                Self::new_with_model(expected_dim, expected_model.to_owned())
            }
        }
    }
}

/// Where [`VectorStore::load_or_empty`] moves an incompatible mirror.
fn quarantine_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".incompatible");
    PathBuf::from(s)
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

/// Dot product of an f32 query against a quantised record — accumulate
/// `q[i] * record.q[i]` then apply the record's scale once at the end.
fn dot_dequant(query: &[f32], r: &Record) -> f32 {
    let sum: f32 = query
        .iter()
        .zip(r.q.iter())
        .map(|(x, y)| x * *y as f32)
        .sum();
    sum * r.scale
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
    fn insert_normalises_then_quantises() {
        let mut s = VectorStore::new(3);
        s.insert("a", vec![3.0, 0.0, 0.0]).unwrap();
        let r = s.get("a").unwrap();
        // Should dequantise back to (1, 0, 0) after L2 normalisation.
        let v = r.dequantized();
        assert!(approx(v[0], 1.0));
        assert!(approx(v[1], 0.0));
        assert!(approx(v[2], 0.0));
        assert_eq!(r.q[0], 127, "unit component maps to full int8 range");
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
        assert!(approx(r.dequantized()[1], 1.0));
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
        // Quantised payloads are byte-identical across the round trip.
        assert_eq!(s.get("a").unwrap().q, back.get("a").unwrap().q);
        assert_eq!(s.get("a").unwrap().scale, back.get("a").unwrap().scale);
    }

    #[test]
    fn quantisation_cosine_error_is_negligible() {
        // B13 acceptance: int8 + per-vector scale loses ~nothing on the
        // similarity ordering. Synthetic but non-axis-aligned data.
        let dim = 64;
        let mk = |seed: u32| -> Vec<f32> {
            (0..dim)
                .map(|i| ((seed * 31 + i as u32 * 7) % 101) as f32 / 101.0 - 0.5)
                .collect()
        };
        let mut s = VectorStore::new(dim as usize);
        for seed in 0..8 {
            s.insert(format!("r{seed}"), mk(seed)).unwrap();
        }
        for seed in 0..8 {
            let exact = l2_normalize(mk(seed));
            let hits = s.query_topk(mk(seed), 1).unwrap();
            // Self-similarity should be ~1.0 within quantisation error.
            assert_eq!(hits[0].id, format!("r{seed}"));
            assert!(
                (hits[0].similarity - 1.0).abs() < 0.01,
                "self-similarity {} for r{seed}",
                hits[0].similarity
            );
            let _ = exact;
        }
    }

    #[test]
    fn v1_files_load_transparently_and_requantise() {
        // Mint a version-1 file (raw f32 records, pre-normalised), then
        // load: records must arrive quantised with similarities intact.
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");
        let v1 = StoreOnDiskV1 {
            version: 1,
            dim: 3,
            records: vec![
                RecordV1 {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0],
                },
                RecordV1 {
                    id: "b".into(),
                    vector: vec![0.0, 1.0, 0.0],
                },
            ],
        };
        std::fs::write(&path, bincode::serialize(&v1).unwrap()).unwrap();

        let s = VectorStore::load(&path, 3).unwrap();
        assert_eq!(s.len(), 2);
        let hits = s.query_topk(vec![1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(hits[0].id, "a");
        assert!(approx(hits[0].similarity, 1.0));

        // Persisting writes the current format; a reload still works and v1
        // is gone. Pinned to the constant, not a literal, so a future version
        // bump doesn't need this assertion rewritten again.
        s.persist(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            FORMAT_VERSION
        );
        assert_eq!(VectorStore::load(&path, 3).unwrap().len(), 2);
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
    fn load_or_empty_preserves_an_incompatible_mirror_instead_of_letting_it_be_overwritten() {
        // This test previously asserted the wipe was fine, on the reasoning
        // that "the next save() will rebuild it". It did not: the next save
        // persisted the empty-plus-one store straight over the file, and the
        // `reindex` the comment leaned on did not exist. A dim change
        // therefore destroyed every stored vector (#136).
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");
        let mut original = VectorStore::new(4);
        original.insert("a", vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        original.insert("b", vec![4.0, 3.0, 2.0, 1.0]).unwrap();
        original.persist(&path).unwrap();
        let original_bytes = std::fs::read(&path).unwrap();

        // Reload at a different width — the config change that used to wipe.
        let fresh = VectorStore::load_or_empty(&path, 8);
        assert!(fresh.is_empty());
        assert_eq!(fresh.dim(), 8);

        // Now do the thing that used to destroy the data.
        fresh.persist(&path).unwrap();

        // The original vectors survive, moved aside rather than overwritten.
        let aside = quarantine_path(&path);
        assert!(
            aside.exists(),
            "the incompatible mirror must be preserved, not destroyed"
        );
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            original_bytes,
            "the preserved mirror must be byte-identical to what was written"
        );
        // ...and it is still a loadable store at its original width.
        let recovered = VectorStore::load(&aside, 4).unwrap();
        assert_eq!(recovered.len(), 2);
    }

    /// #136 — a mirror must record which embedder produced it, so a model
    /// swap at the SAME width can be detected. Without the stamp, prior
    /// vectors stayed in place and were silently compared against vectors
    /// from a different model forever.
    #[test]
    fn a_model_swap_at_the_same_dim_is_detected_rather_than_silently_mixed() {
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");

        let mut original = VectorStore::new_with_model(4, "model-a".to_owned());
        original.insert("a", vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        original.persist(&path).unwrap();
        let original_bytes = std::fs::read(&path).unwrap();

        // Same embedder reloads cleanly.
        assert_eq!(
            VectorStore::load_expecting(&path, 4, "model-a")
                .unwrap()
                .len(),
            1
        );

        // A different embedder at the same width — the case that used to be
        // completely invisible.
        let err = VectorStore::load_expecting(&path, 4, "model-b")
            .expect_err("a mirror from another embedder must not load silently");
        assert!(
            matches!(err, VectorStoreError::LoadedModelMismatch { .. }),
            "expected LoadedModelMismatch, got {err:?}"
        );

        // The incompatible mirror is preserved, not left to be overwritten.
        let fresh = VectorStore::load_or_empty_expecting(&path, 4, "model-b");
        assert!(fresh.is_empty());
        assert_eq!(fresh.embed_model(), "model-b");
        let aside = quarantine_path(&path);
        assert!(aside.exists(), "the mismatched mirror must be preserved");
        assert_eq!(std::fs::read(&aside).unwrap(), original_bytes);
    }

    /// A pre-#136 (v2) mirror has no model stamp. It must still load — we
    /// migrate rather than discard — and get stamped on the next persist.
    #[test]
    fn a_pre_stamp_v2_mirror_loads_and_is_stamped_on_the_next_persist() {
        let td = tempdir().unwrap();
        let path = td.path().join("vec.bin");

        // Mint a v2 (model-less) envelope by hand.
        let mut store = VectorStore::new_with_model(4, "model-a".to_owned());
        store.insert("a", vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let v2 = StoreOnDiskV2 {
            version: 2,
            dim: 4,
            records: store.records.clone(),
        };
        std::fs::write(&path, bincode::serialize(&v2).unwrap()).unwrap();

        // An unstamped mirror loads under ANY expected model — we cannot know
        // which embedder wrote it, so we adopt the current one rather than
        // discarding vectors on a guess. This blind spot exists exactly once,
        // for stores written before the field existed.
        let loaded =
            VectorStore::load_expecting(&path, 4, "model-b").expect("a v2 mirror must still load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.embed_model(), "model-b");

        // Once re-persisted it IS stamped, so the next swap is detectable.
        loaded.persist(&path).unwrap();
        assert!(VectorStore::load_expecting(&path, 4, "model-c").is_err());
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
