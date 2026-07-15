//! Content-hash manifest — incremental reuse of the persisted index (P3).
//!
//! `index/manifest.json` records, per indexed file, a content hash + the
//! `(mtime, size)` fast-path metadata + the chunk ids that file owns. A delta
//! pass diffs the live folder against it to re-chunk+re-embed **only** changed
//! or new files, drop deleted files' chunks, and keep every unchanged vector
//! verbatim — driven by **content hash**, not just mtime, so a `touch` or a
//! `git checkout` that moves mtime without changing bytes no longer triggers a
//! wasteful re-embed (the mtime-only [`super::plan_delta`]-era behaviour).
//!
//! ## Schema (`index/manifest.json`)
//!
//! ```jsonc
//! {
//!   "version": 1,                       // bump ⇒ force a one-time full rebuild
//!   "embed_model": "nomic-embed-text",  // model/dim guard (§3.4)
//!   "embed_dim": 768,
//!   "files": {
//!     "<canonical_path>": {
//!       "hash": "<sha256(bytes) hex[..16]>",  // "" when unknown (legacy seed)
//!       "size": 12345,
//!       "mtime": 1700000000.0,
//!       "chunk_ids": ["abcd…", "ef01…"],
//!       "chunk_count": 2
//!     }
//!   }
//! }
//! ```
//!
//! Keyed by the **same canonical path** the chunks use ([`super::walk::canonical_path`]),
//! so manifest ↔ chunk lookups always agree.
//!
//! ## Atomicity (§3.3)
//!
//! The invariant is enforced by the caller, not here: **`chunks.jsonl` is
//! written first, `manifest.json` second**, both under the per-workspace
//! [`super::lock`]. A crash between leaves the manifest *behind* the chunks —
//! the next pass re-embeds the already-embedded files (wasteful but correct,
//! idempotent). The reverse (manifest ahead of chunks) would skip a needed
//! embed and is made impossible by the ordering.
//!
//! ## Model/dim guard (§3.4)
//!
//! [`Manifest::is_compatible`] compares the stored `embed_model`/`embed_dim`
//! against the current config; a mismatch forces a full rebuild so swapping the
//! embedding model can't silently mix incompatible vectors into `chunks.jsonl`.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::common::{embed_dim, embed_model};

use super::store::{index_dir, IndexedChunk};
use super::walk::FileStat;

/// Manifest schema version. Bump to force a one-time full rebuild across the
/// installed base (an incompatible manifest ⇒ [`needs_full_rebuild`]).
pub const MANIFEST_VERSION: u32 = 1;

/// mtime comparison tolerance (seconds) — matches the chunk-store discipline.
const MTIME_TOL: f64 = 0.001;

/// One file's manifest record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct FileEntry {
    /// `sha256(file bytes)` truncated to 16 hex chars. `""` when unknown — a
    /// legacy entry seeded from pre-manifest chunks carries no hash until the
    /// file is next read.
    pub hash: String,
    /// File size in bytes (the `(mtime, size)` fast-path's size half).
    pub size: u64,
    /// File mtime (epoch seconds) at the recorded hash.
    pub mtime: f64,
    /// The [`IndexedChunk`] ids this file owns, in `chunk_idx` order.
    #[serde(default)]
    pub chunk_ids: Vec<String>,
    /// `chunk_ids.len()` (denormalised for a cheap read).
    pub chunk_count: u32,
}

/// The per-workspace content-hash manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// Schema version ([`MANIFEST_VERSION`]).
    pub version: u32,
    /// The embedding model the vectors were produced with.
    pub embed_model: String,
    /// The embedding dimension the vectors were produced at.
    pub embed_dim: usize,
    /// Per-file records, keyed by canonical path.
    #[serde(default)]
    pub files: BTreeMap<String, FileEntry>,
}

impl Manifest {
    /// A fresh, empty manifest stamped with the **current** embed model/dim.
    pub fn current_env() -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            embed_model: embed_model(),
            embed_dim: embed_dim(),
            files: BTreeMap::new(),
        }
    }

    /// True iff this manifest's version + embed model/dim match the current
    /// config — i.e. its vectors are reusable. A mismatch forces a full rebuild
    /// (§3.4): bumping [`MANIFEST_VERSION`], swapping `WYLDE_EMBED_MODEL`, or
    /// changing `WYLDE_EMBED_DIM` all invalidate the persisted vectors.
    pub fn is_compatible(&self) -> bool {
        self.version == MANIFEST_VERSION
            && self.embed_model == embed_model()
            && self.embed_dim == embed_dim()
    }
}

/// `<data_dir>/workspaces/<id>/index/manifest.json`.
fn manifest_path(workspace_id: &str) -> std::path::PathBuf {
    index_dir(workspace_id).join("manifest.json")
}

/// Load the manifest. `None` on a missing/torn file (a legacy index, or a
/// never-indexed workspace).
pub fn load(workspace_id: &str) -> Option<Manifest> {
    let raw = std::fs::read_to_string(manifest_path(workspace_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Atomically write the manifest (tmp + rename), creating `index/` if needed.
/// MUST be called **after** the matching `chunks.jsonl` write (§3.3).
pub fn save(workspace_id: &str, manifest: &Manifest) -> std::io::Result<()> {
    let dir = index_dir(workspace_id);
    crate::common::ensure_dir(&dir)?;
    let path = manifest_path(workspace_id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(manifest).unwrap())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Whether an existing index must be fully rebuilt rather than delta-updated:
/// a present-but-incompatible manifest (version / model / dim mismatch, §3.4).
/// An **absent** manifest is *not* a forced rebuild — a legacy index upgrades
/// in place via the delta's mtime fallback ([`diff`]), so no mass re-embed.
pub fn needs_full_rebuild(workspace_id: &str) -> bool {
    match load(workspace_id) {
        Some(m) => !m.is_compatible(),
        None => false,
    }
}

/// `(hash, size, mtime)` for one file — the fields a [`FileEntry`] carries
/// minus the chunk ids (filled once the chunks exist).
type FileMeta = (String, u64, f64);

/// The keep / re-embed / delete split a delta pass computes from the prior
/// manifest + a metadata-only folder walk. Pure — no IO; the hashing is
/// injected — so the diff logic is unit-testable without a live embedder.
#[derive(Debug, Default, PartialEq)]
pub struct ManifestDiff {
    /// Live paths whose existing chunks are kept as-is (unchanged or touched).
    pub keep_paths: HashSet<String>,
    /// Live paths to re-chunk + re-embed (changed or new).
    pub to_embed: Vec<String>,
    /// Paths that had chunks/manifest but vanished from the folder — drop their
    /// chunks + graph-clean.
    pub deleted: Vec<String>,
    /// `(hash, size, mtime)` for every live path (kept + to_embed), so the next
    /// manifest can be assembled. Chunk ids are filled later from the chunks.
    pub file_meta: BTreeMap<String, FileMeta>,
}

/// Diff the live folder against the prior manifest.
///
/// * `prior` — the loaded manifest (use [`Manifest::current_env`] when absent).
/// * `stats` — the metadata-only walk ([`super::walk::walk_file_stats`]).
/// * `legacy_mtimes` — max cached-chunk mtime per path, for the **upgrade
///   transition**: a path with chunks but no manifest entry (a pre-P3 index)
///   falls back to the old mtime-vs-cached compare so it isn't mass-re-embedded.
/// * `hash_fn` — reads + hashes one file; only called when the `(mtime, size)`
///   fast-path can't decide. `None` ⇒ unreadable ⇒ kept as unchanged.
///
/// Decision per live file:
/// 1. manifest entry + `(mtime, size)` match → **unchanged** (no read).
/// 2. else hash: equals manifest hash → **touched** (keep, refresh mtime; no
///    re-embed); differs → **changed**; no manifest entry + cached chunks +
///    mtime not newer → **unchanged** (legacy keep); otherwise → **new/changed**.
pub fn diff<F>(
    prior: &Manifest,
    stats: &[FileStat],
    legacy_mtimes: &HashMap<String, f64>,
    hash_fn: F,
) -> ManifestDiff
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = ManifestDiff::default();
    let live: HashSet<&str> = stats.iter().map(|s| s.path.as_str()).collect();

    for s in stats {
        match prior.files.get(&s.path) {
            // ── Known to the manifest ────────────────────────────────────
            Some(entry) => {
                let fast_path_match =
                    entry.size == s.size && (entry.mtime - s.mtime).abs() <= MTIME_TOL;
                if fast_path_match {
                    // Unchanged — no read, no embed.
                    out.keep_paths.insert(s.path.clone());
                    out.file_meta.insert(
                        s.path.clone(),
                        (entry.hash.clone(), entry.size, entry.mtime),
                    );
                    continue;
                }
                match hash_fn(&s.path) {
                    Some(h) if !entry.hash.is_empty() && h == entry.hash => {
                        // Touched (mtime/size moved, bytes identical) — keep the
                        // vectors, refresh the recorded mtime/size. No re-embed.
                        out.keep_paths.insert(s.path.clone());
                        out.file_meta.insert(s.path.clone(), (h, s.size, s.mtime));
                    }
                    Some(h) => {
                        // Real content change.
                        out.to_embed.push(s.path.clone());
                        out.file_meta.insert(s.path.clone(), (h, s.size, s.mtime));
                    }
                    None => {
                        // Unreadable now — keep the prior vectors rather than
                        // lose them.
                        out.keep_paths.insert(s.path.clone());
                        out.file_meta.insert(
                            s.path.clone(),
                            (entry.hash.clone(), entry.size, entry.mtime),
                        );
                    }
                }
            }
            // ── Not in the manifest ──────────────────────────────────────
            None => match legacy_mtimes.get(&s.path) {
                // Pre-P3 index: a cached chunk exists. Old mtime rule — keep
                // when the cached mtime isn't older than the file (no re-embed),
                // recording a hashless entry so the next pass can hash on drift.
                Some(cached) if *cached >= s.mtime - MTIME_TOL => {
                    out.keep_paths.insert(s.path.clone());
                    out.file_meta
                        .insert(s.path.clone(), (String::new(), s.size, s.mtime));
                }
                // New file (or a legacy file that changed since its cache).
                _ => {
                    let h = hash_fn(&s.path).unwrap_or_default();
                    out.to_embed.push(s.path.clone());
                    out.file_meta.insert(s.path.clone(), (h, s.size, s.mtime));
                }
            },
        }
    }

    // Deleted = anything the manifest OR the cached chunks knew that is no
    // longer in the live folder.
    let mut gone: HashSet<String> = HashSet::new();
    for p in prior.files.keys() {
        if !live.contains(p.as_str()) {
            gone.insert(p.clone());
        }
    }
    for p in legacy_mtimes.keys() {
        if !live.contains(p.as_str()) {
            gone.insert(p.clone());
        }
    }
    out.deleted = gone.into_iter().collect();
    out.deleted.sort();
    out
}

/// Assemble the next manifest from the diff's per-file metadata + the final
/// merged chunk set (so `chunk_ids`/`chunk_count` reflect what was actually
/// persisted). Stamped with the current embed model/dim.
pub fn build(file_meta: &BTreeMap<String, FileMeta>, chunks: &[IndexedChunk]) -> Manifest {
    // Group chunk ids by path in chunk_idx order.
    let mut by_path: BTreeMap<&str, Vec<(u32, &str)>> = BTreeMap::new();
    for c in chunks {
        by_path
            .entry(c.path.as_str())
            .or_default()
            .push((c.chunk_idx, c.id.as_str()));
    }
    let mut files = BTreeMap::new();
    for (path, (hash, size, mtime)) in file_meta {
        let mut ids: Vec<String> = by_path
            .get(path.as_str())
            .map(|v| {
                let mut v = v.clone();
                v.sort_by_key(|(idx, _)| *idx);
                v.into_iter().map(|(_, id)| id.to_owned()).collect()
            })
            .unwrap_or_default();
        ids.shrink_to_fit();
        let chunk_count = ids.len() as u32;
        files.insert(
            path.clone(),
            FileEntry {
                hash: hash.clone(),
                size: *size,
                mtime: *mtime,
                chunk_ids: ids,
                chunk_count,
            },
        );
    }
    Manifest {
        version: MANIFEST_VERSION,
        embed_model: embed_model(),
        embed_dim: embed_dim(),
        files,
    }
}

/// Build a manifest straight from a chunk set (no prior metadata) — the
/// full-rebuild path. Hashes each distinct file once so subsequent deltas get
/// the `(mtime, size)` fast-path + hash confirm. Unreadable files record a
/// hashless entry (still keyed, so the next pass can hash on drift).
pub fn build_full(chunks: &[IndexedChunk]) -> Manifest {
    let mut meta: BTreeMap<String, FileMeta> = BTreeMap::new();
    let mut paths: Vec<&str> = chunks.iter().map(|c| c.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    for p in paths {
        let (hash, size, mtime) = super::walk::hash_file(p).unwrap_or_default();
        meta.insert(p.to_owned(), (hash, size, mtime));
    }
    build(&meta, chunks)
}

/// The recorded content hash for one file, if the manifest knows it. Used by
/// the watcher's per-file [`super::delta::upsert_file`] to short-circuit a
/// touch/no-op save (hash unchanged ⇒ no re-embed). `None` for an absent
/// manifest (legacy index) or an unknown file.
pub fn file_hash(workspace_id: &str, path: &str) -> Option<String> {
    load(workspace_id)?.files.get(path).map(|e| e.hash.clone())
}

/// Upsert one file's manifest entry (watcher per-file delta). MUST be called
/// under the per-workspace index [`super::lock`], **after** the matching chunk
/// write (§3.3). Creates the manifest from the current env if absent.
pub fn update_file(
    workspace_id: &str,
    path: &str,
    hash: String,
    size: u64,
    mtime: f64,
    chunk_ids: Vec<String>,
) {
    let mut m = load(workspace_id).unwrap_or_else(Manifest::current_env);
    let chunk_count = chunk_ids.len() as u32;
    m.files.insert(
        path.to_owned(),
        FileEntry {
            hash,
            size,
            mtime,
            chunk_ids,
            chunk_count,
        },
    );
    if let Err(e) = save(workspace_id, &m) {
        tracing::warn!("workspaces.rag: manifest update_file failed for {workspace_id}: {e}");
    }
}

/// Refresh just the `(size, mtime)` of an existing entry without touching its
/// hash/chunk ids — the touch/no-op short-circuit. No-op if the entry is absent.
/// Call under the index lock.
pub fn touch_file(workspace_id: &str, path: &str, size: u64, mtime: f64) {
    let Some(mut m) = load(workspace_id) else {
        return;
    };
    if let Some(e) = m.files.get_mut(path) {
        e.size = size;
        e.mtime = mtime;
        let _ = save(workspace_id, &m);
    }
}

/// Drop a file's manifest entry — exact path or anything under
/// `<canonical><sep>` (a deleted directory's subtree). Call under the index
/// lock, after the matching chunk removal.
pub fn remove_files(workspace_id: &str, canonical: &str) {
    let Some(mut m) = load(workspace_id) else {
        return;
    };
    let prefix = format!("{canonical}{}", std::path::MAIN_SEPARATOR);
    let before = m.files.len();
    m.files
        .retain(|p, _| p != canonical && !p.starts_with(&prefix));
    if m.files.len() != before {
        let _ = save(workspace_id, &m);
    }
}

/// The chunk ids the manifest records for an exact path **or** anything under
/// `<canonical><sep>` (a directory subtree) — the delete keys the watcher's
/// lexical remove ([`super::lexical::sync_remove_file`]) needs to drop a removed
/// file or directory from the BM25 index. `Vec::new()` for an absent manifest
/// (legacy index) or an unknown path. Read BEFORE [`remove_files`] mutates the
/// manifest.
pub fn chunk_ids_under(workspace_id: &str, canonical: &str) -> Vec<String> {
    let Some(m) = load(workspace_id) else {
        return Vec::new();
    };
    let prefix = format!("{canonical}{}", std::path::MAIN_SEPARATOR);
    let mut ids = Vec::new();
    for (p, e) in &m.files {
        if p == canonical || p.starts_with(&prefix) {
            ids.extend(e.chunk_ids.iter().cloned());
        }
    }
    ids
}

/// Max cached-chunk mtime per path — the `legacy_mtimes` input to [`diff`] for
/// a pre-manifest index (mirrors the old `plan_delta` cached-mtime map).
pub fn legacy_mtimes(chunks: &[IndexedChunk]) -> HashMap<String, f64> {
    let mut m: HashMap<String, f64> = HashMap::new();
    for c in chunks {
        let e = m.entry(c.path.clone()).or_insert(c.mtime);
        if c.mtime > *e {
            *e = c.mtime;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(path: &str, mtime: f64, size: u64) -> FileStat {
        FileStat {
            path: path.to_owned(),
            mtime,
            size,
        }
    }

    fn entry(hash: &str, size: u64, mtime: f64, ids: &[&str]) -> FileEntry {
        FileEntry {
            hash: hash.to_owned(),
            size,
            mtime,
            chunk_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            chunk_count: ids.len() as u32,
        }
    }

    fn manifest_with(files: &[(&str, FileEntry)]) -> Manifest {
        let mut m = Manifest::current_env();
        for (p, e) in files {
            m.files.insert((*p).to_owned(), e.clone());
        }
        m
    }

    #[test]
    fn unchanged_file_takes_mtime_size_fast_path_without_hashing() {
        let prior = manifest_with(&[("/a.rs", entry("hashA", 10, 100.0, &["c0"]))]);
        let stats = vec![stat("/a.rs", 100.0, 10)];
        // hash_fn must NOT be called on the fast path — panic if it is.
        let d = diff(&prior, &stats, &HashMap::new(), |_| {
            panic!("hash_fn called on the (mtime,size) fast path")
        });
        assert!(d.keep_paths.contains("/a.rs"));
        assert!(d.to_embed.is_empty());
        assert_eq!(d.file_meta["/a.rs"], ("hashA".to_owned(), 10, 100.0));
    }

    #[test]
    fn touched_file_same_bytes_keeps_without_reembed() {
        // mtime moved (touch / checkout) but the hash is identical.
        let prior = manifest_with(&[("/a.rs", entry("hashA", 10, 100.0, &["c0"]))]);
        let stats = vec![stat("/a.rs", 999.0, 10)];
        let d = diff(&prior, &stats, &HashMap::new(), |_| {
            Some("hashA".to_owned())
        });
        assert!(d.keep_paths.contains("/a.rs"), "touched file is kept");
        assert!(d.to_embed.is_empty(), "no re-embed for identical bytes");
        // mtime refreshed in the recorded meta.
        assert_eq!(d.file_meta["/a.rs"], ("hashA".to_owned(), 10, 999.0));
    }

    #[test]
    fn changed_bytes_get_reembedded() {
        let prior = manifest_with(&[("/a.rs", entry("hashA", 10, 100.0, &["c0"]))]);
        let stats = vec![stat("/a.rs", 200.0, 12)];
        let d = diff(&prior, &stats, &HashMap::new(), |_| {
            Some("hashB".to_owned())
        });
        assert_eq!(d.to_embed, vec!["/a.rs".to_owned()]);
        assert!(!d.keep_paths.contains("/a.rs"));
        assert_eq!(d.file_meta["/a.rs"], ("hashB".to_owned(), 12, 200.0));
    }

    #[test]
    fn new_file_is_embedded() {
        let prior = Manifest::current_env();
        let stats = vec![stat("/new.rs", 50.0, 5)];
        let d = diff(&prior, &stats, &HashMap::new(), |_| {
            Some("hashN".to_owned())
        });
        assert_eq!(d.to_embed, vec!["/new.rs".to_owned()]);
        assert_eq!(d.file_meta["/new.rs"], ("hashN".to_owned(), 5, 50.0));
    }

    #[test]
    fn deleted_file_is_dropped_and_flagged() {
        let prior = manifest_with(&[
            ("/keep.rs", entry("hk", 10, 100.0, &["k0"])),
            ("/gone.rs", entry("hg", 20, 100.0, &["g0"])),
        ]);
        let stats = vec![stat("/keep.rs", 100.0, 10)];
        let d = diff(&prior, &stats, &HashMap::new(), |_| Some("x".to_owned()));
        assert_eq!(d.deleted, vec!["/gone.rs".to_owned()]);
        assert!(d.keep_paths.contains("/keep.rs"));
        assert!(
            !d.file_meta.contains_key("/gone.rs"),
            "deleted file dropped"
        );
    }

    #[test]
    fn legacy_index_without_manifest_does_not_mass_reembed() {
        // No manifest entry, but a cached chunk whose mtime isn't older than the
        // file ⇒ keep (upgrade transition), record a hashless entry.
        let prior = Manifest::current_env();
        let mut legacy = HashMap::new();
        legacy.insert("/a.rs".to_owned(), 100.0);
        let stats = vec![stat("/a.rs", 100.0, 10)];
        let d = diff(&prior, &stats, &legacy, |_| {
            panic!("legacy unchanged must not hash")
        });
        assert!(d.keep_paths.contains("/a.rs"));
        assert!(d.to_embed.is_empty());
        assert_eq!(d.file_meta["/a.rs"], (String::new(), 10, 100.0));
    }

    #[test]
    fn legacy_file_changed_since_cache_is_reembedded() {
        let prior = Manifest::current_env();
        let mut legacy = HashMap::new();
        legacy.insert("/a.rs".to_owned(), 100.0); // cached older than the file
        let stats = vec![stat("/a.rs", 500.0, 10)];
        let d = diff(&prior, &stats, &legacy, |_| Some("hnew".to_owned()));
        assert_eq!(d.to_embed, vec!["/a.rs".to_owned()]);
        assert_eq!(d.file_meta["/a.rs"], ("hnew".to_owned(), 10, 500.0));
    }

    #[test]
    fn build_groups_chunk_ids_by_path_in_order() {
        let mut meta: BTreeMap<String, FileMeta> = BTreeMap::new();
        meta.insert("/a.rs".to_owned(), ("hA".to_owned(), 10, 100.0));
        meta.insert("/b.rs".to_owned(), ("hB".to_owned(), 20, 200.0));
        let chunks = vec![
            IndexedChunk {
                id: "a1".into(),
                path: "/a.rs".into(),
                chunk_idx: 1,
                content: String::new(),
                mtime: 100.0,
                start_line: 1,
                end_line: 1,
                vector: vec![0.1],
            },
            IndexedChunk {
                id: "a0".into(),
                path: "/a.rs".into(),
                chunk_idx: 0,
                content: String::new(),
                mtime: 100.0,
                start_line: 1,
                end_line: 1,
                vector: vec![0.1],
            },
        ];
        let m = build(&meta, &chunks);
        assert_eq!(
            m.files["/a.rs"].chunk_ids,
            vec!["a0", "a1"],
            "ordered by idx"
        );
        assert_eq!(m.files["/a.rs"].chunk_count, 2);
        assert_eq!(m.files["/b.rs"].chunk_count, 0, "no chunks ⇒ empty entry");
        assert!(m.is_compatible());
    }

    #[test]
    fn incompatible_model_or_dim_forces_rebuild() {
        let mut m = Manifest::current_env();
        // Same env ⇒ compatible.
        assert!(m.is_compatible());
        // A different model ⇒ incompatible (forces a full rebuild).
        m.embed_model = "some-other-model".into();
        assert!(!m.is_compatible());
        let mut m2 = Manifest::current_env();
        m2.embed_dim += 1;
        assert!(!m2.is_compatible());
        let mut m3 = Manifest::current_env();
        m3.version = MANIFEST_VERSION + 1;
        assert!(!m3.is_compatible());
    }
}
