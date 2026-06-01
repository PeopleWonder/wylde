---
title: Extending the vector store
audience: contributors changing similarity search, persistence, or swapping algorithms
authored: 2026-05-27
status: living reference
---

# The vector store

## Executive summary

A vector store is the engine that powers "find things similar to this."
When you ask Wylde "what notes do I have about the migration plan,"
your query gets turned into a list of numbers (an "embedding," ~768
floats), every memory has its own list of numbers, and the vector
store finds the memories whose numbers are closest to yours. The
geometry is called cosine similarity; you don't need to know how it
works, just that it gives a score between -1 (totally unrelated) and 1
(near-identical).

Wylde's vector store is **deliberately small and simple**. It's a few
hundred lines of pure Rust that holds every embedding in memory in a
plain `Vec`, walks the whole list on every query, and serialises to
one bincode file on disk. There's no HNSW index, no graph-based
approximate-nearest-neighbour algorithm, no Lucene-style segment
merging. That sounds primitive, but it's the right choice for Wylde's
scale: the curated long-term tier has low thousands of records, and at
that scale a linear scan beats every clever index in build time,
query latency, and determinism. If the scale ever balloons (millions
of records), the trait-based extension path is documented below for
swapping in HNSW behind the same surface.

This doc explains the data structure, the on-disk format (and how to
bump its version), the cosine-similarity-via-pre-normalisation trick
that keeps the inner loop a dot product, and how to swap algorithms
when you actually need to.

## How it works

### Files

```
rust/crates/wylde-harness/src/memory/vector/
└── mod.rs           — VectorStore, Record, Hit, StoreOnDisk, ~517 LOC
```

One file, no submodules. Deliberately compact.

### Public types

* **`VectorStore`** — the store itself. Owns `dim: usize` (fixed at
  construction) + `records: Vec<Record>`.
* **`Record`** — `{ id: String, vector: Vec<f32> }`. The pre-normalised
  embedding. Public so callers can construct records for batch loads.
* **`Hit`** — `{ id: String, similarity: f32 }`. Returned by
  `query_topk`. Cosine similarity in [-1.0, 1.0].
* **`VectorStoreError`** — `DimMismatch`, `EmptyVector`,
  `LoadedDimMismatch`, `UnsupportedVersion`, `Io`, `Bincode`.

### Public methods

```rust
pub fn new(dim: usize) -> Self
pub fn dim(&self) -> usize
pub fn len(&self) -> usize
pub fn is_empty(&self) -> bool
pub fn insert(&mut self, id: impl Into<String>, vector: Vec<f32>)
    -> Result<(), VectorStoreError>
pub fn delete(&mut self, id: &str) -> bool
pub fn query_topk(&self, query: &[f32], k: usize)
    -> Result<Vec<Hit>, VectorStoreError>
pub fn persist(&self, path: &Path) -> Result<(), VectorStoreError>
pub fn load(path: &Path, dim: usize)
    -> Result<Self, VectorStoreError>
pub fn load_or_empty(path: &Path, dim: usize)
    -> Result<Self, VectorStoreError>
```

That's the whole surface. `load_or_empty` is a convenience for first-run
where the file doesn't exist yet — returns `VectorStore::new(dim)`
rather than an error.

### The cosine-as-dot-product trick

Cosine similarity between two vectors `a` and `b` is `(a · b) / (||a||
* ||b||)`. The store **L2-normalises every vector on insert** —
divides by its own L2 norm so `||v|| == 1`. After that, cosine
similarity reduces to just the dot product `a · b`, which is a single
loop with no division. The query vector is normalised the same way
before the scan starts.

The trade-off: queries are slightly faster, but you can't get raw
vectors back out for downstream math. If you need the raw vector for
some other purpose, you have to store it elsewhere (the JSON
authoritative source in long_term, say). This is fine — `vector/` is
specifically the *search* substrate; storage of the raw is upstream's
problem.

### On-disk format (versioned)

```rust
struct StoreOnDisk {
    version: u32,   // currently 1
    dim: u32,
    records: Vec<Record>,
}
```

Bincode default — little-endian, fixed-int width. The byte layout is
deterministic, so a checksum of the file is meaningful. Atomic writes
via `<path>.tmp` + `rename(path.tmp, path)` so a crash mid-persist
leaves the prior good file in place.

### Why not HNSW

HNSW (Hierarchical Navigable Small World graphs) is the standard
approximate-nearest-neighbour algorithm for large vector sets. It's
faster at scale but:

* **Slow to build** at small sizes; the index construction has fixed
  per-record overhead that pays off only above ~100k records.
* **Stochastic** — the graph build depends on randomness, so two
  identical inputs can produce different indexes. Tests have to be
  approximate-match.
* **Not pluggable mid-flight** — adding to an HNSW means recomputing
  edges; deleting is even harder (most implementations don't really
  delete, just mark tombstones).
* **Heavier deps** — `instant-distance`, `hora`, or `hnsw_rs`, each
  with its own quirks.

At Wylde's scale, the linear scan over normalised vectors is faster
*and* simpler. Reconsider HNSW only when query latency on the linear
scan exceeds tens of milliseconds on cold cache — that's order of
~10k+ records of dim 768.

### Concurrency

The store itself is `Send + Sync` (no interior mutability) but
**not** `Clone`-cheap (the `Vec<Record>` is the dominant cost).
Consumers wrap it in `Mutex<VectorStore>` or `RwLock<VectorStore>`
themselves; long_term + RAG both use a `Mutex` because their write
paths re-acquire on every save.

## How to extend

### Add a metadata blob to records

Suppose you want to attach per-record metadata (e.g. tier tag, source
path) so the store can return them with hits without a follow-up
lookup. Two paths:

**Cheap (recommended):** keep `Record` minimal; let the caller hold a
parallel `HashMap<id, Metadata>`. Long_term already does this — JSON is
authoritative for everything except the embedding.

**Expensive:** extend `Record` with a `metadata: Value` field. Bump
`FORMAT_VERSION` from 1 to 2. Write a migration in `load` that handles
both version 1 (no metadata) and version 2 (with). The migration is
forward-only — version 2 files cannot be read by older binaries.

```rust
const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub id: String,
    pub vector: Vec<f32>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

// In load():
match envelope.version {
    1 => /* records have no metadata; fill with Null */,
    2 => /* records have metadata field */,
    v => return Err(VectorStoreError::UnsupportedVersion(v)),
}
```

Don't skip the migration. Production stores have months of data; a
version bump without a migration means a hard reindex.

### Swap to HNSW

The current code is monolithic (no trait surface). Adding HNSW means
introducing one:

```rust
pub trait Searcher: Send + Sync {
    fn insert(&mut self, id: String, vector: Vec<f32>)
        -> Result<(), VectorStoreError>;
    fn delete(&mut self, id: &str) -> bool;
    fn query_topk(&self, query: &[f32], k: usize)
        -> Result<Vec<Hit>, VectorStoreError>;
}

pub struct LinearSearcher { /* current impl */ }
pub struct HnswSearcher { /* new impl wrapping a crate */ }

pub struct VectorStore {
    dim: usize,
    searcher: Box<dyn Searcher>,
}
```

Pick the searcher at construction (`VectorStore::new_linear(dim)` vs
`VectorStore::new_hnsw(dim, params)`). Persistence becomes
algorithm-specific — HNSW needs its own serialise/deserialise. The
on-disk envelope version bumps; `version: 1` means linear, `version:
2` means HNSW.

Stochasticity is the gotcha — HNSW builds depend on RNG seeds. Pin a
seed; the test suite still has tolerance windows for the few queries
where the result order legitimately varies.

### Add a new similarity metric

Cosine is the only metric today. Euclidean distance or Manhattan
distance would mean dropping the pre-normalisation trick and writing
the full metric inside `query_topk`. Easy code change; harder semantic
change — every consumer assumes "higher hit.similarity = more similar"
and a distance metric inverts that. If you add a metric, return
*similarity* (1 - normalised_distance) rather than raw distance, so
downstream code keeps working.

### Persist on every write

Currently the consumer (long_term, RAG) decides when to call
`persist`. If you want a "auto-persist on every insert" mode:

```rust
pub fn insert_and_persist(
    &mut self,
    id: impl Into<String>,
    vector: Vec<f32>,
    path: &Path,
) -> Result<(), VectorStoreError> {
    self.insert(id, vector)?;
    self.persist(path)
}
```

Cost: every insert does a full file rewrite. For Wylde's scale that's
sub-ms; for a million-record store it'd be measured in seconds.

## Gotchas

* **`dim` is fixed at construction.** A 768-dim store can't accept a
  1536-dim vector — you get `DimMismatch`. Switching embedder model
  means rebuilding the entire store. This is intentional; mixing dims
  in one store is meaningless math.
* **Empty vectors are rejected.** `EmptyVector` error on `insert(id,
  vec![])`. There's no "zero vector" sentinel meaning "no embedding
  yet" — store metadata elsewhere if you need that signalling.
* **Persist is atomic but not durable-on-crash.** `rename` survives an
  OS crash on most filesystems but not all. If durability matters more
  than throughput, add an `fsync(persist_path)` after the rename.
* **`query_topk(query, 0)` returns an empty list, not an error.** This
  is so callers can pass `limit` without a special case for "user
  asked for nothing."
* **The pre-normalisation modifies the input vector.** `insert` takes
  `vector: Vec<f32>` (owned) and normalises in place. If the caller
  needs the raw vector back, they have to clone before insert. This
  is documented in the method comment but easy to miss.
* **`UnsupportedVersion` is forward-incompatible.** A store written by
  version 2 cannot be read by version 1. There's no "downgrade path"
  — once you bump format, you commit. Tag old format binaries clearly.

## Cross-links

* [index.md](./index.md) — memory subsystem overview.
* [long-term.md](./long-term.md) — primary consumer of the vector
  store today.
* [rag.md](./rag.md) — secondary consumer (one store per tier).
* `~/.claude/projects/.../memory/wylde_phase7b_long_term_shipped.md` —
  the Phase 7.B-1 ship report, explains why bincode and linear cosine
  over alternatives.

---

*Small and simple is the feature. Resist the urge to "improve" the
store unless real query latency is hurting.*
