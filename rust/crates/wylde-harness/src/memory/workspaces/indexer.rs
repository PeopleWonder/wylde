//! File indexer for workspace RAG.
//!
//! Rust port of `Core/harness/memory/workspaces/_index.py`. Walks a
//! workspace folder, chunks text-ish files, embeds the chunks, and
//! persists them in a per-workspace vector store + metadata sidecar.
//!
//! ## On-disk layout
//!
//! Per workspace at `<data_dir>/indexes/<workspace_id>/`:
//!
//! ```text
//! files.bin       — VectorStore (chunk_id → embedding)
//! files.meta.json — {"rows": [IndexRow ...]} sidecar
//! ```
//!
//! The Python implementation uses LanceDB (`files.lance/` folder). The
//! Rust port substitutes a `(VectorStore, JSON sidecar)` pair for the
//! same reason long-term does: pure-Rust deps, dim-agnostic, low
//! thousands of chunks per workspace, no JNI / native client. The on-
//! disk layout differs — operators reindexing post-cutover get
//! mtime-cheap delta passes; nothing else cares about the wire shape.
//!
//! ## Two entry points
//!
//! * [`index_full`] drops the existing store and re-embeds everything.
//! * [`index_delta`] re-embeds only files whose mtime is newer than
//!   the cached row. Faster for everyday `activate` re-runs.
//!
//! Both honour the same skip-rules: hidden files / `.git`-style
//! dirs / binary-blob suffixes / empty / oversized / NUL-byte-sniff.
//!
//! ## Embeddings
//!
//! [`index_full`] / [`index_delta`] call into
//! [`crate::memory::embeddings`] (Seam 1 of the cleanup slice). When
//! the embedder is unreachable, indexing degrades to "write rows, skip
//! vectors" so the metadata sidecar still tracks the file set — the
//! next successful indexer pass fills the vectors in.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{
    get_workspace, indexes_dir, set_indexing_flag, update_file_count, Workspace,
};
use crate::memory::common::{embed_dim, ensure_dir};
use crate::memory::embeddings;
use crate::memory::vector::VectorStore;

/// Soft cap on per-file size. Larger files are skipped. Mirrors Python
/// `_MAX_INDEXABLE_BYTES`.
pub const MAX_INDEXABLE_BYTES: u64 = 1024 * 1024;
/// Chunk width in Python characters (Unicode codepoints). Matches
/// Python `_CHUNK_SIZE_CHARS`.
pub const CHUNK_SIZE_CHARS: usize = 4000;
/// Overlap between adjacent chunks. Matches Python
/// `_CHUNK_OVERLAP_CHARS`.
pub const CHUNK_OVERLAP_CHARS: usize = 200;

/// File suffixes (lowercase, with leading `.`) we never try to read.
/// Mirrors Python `_SKIP_SUFFIXES`.
pub const SKIP_SUFFIXES: &[&str] = &[
    ".pyc", ".pyo", ".class", ".o", ".obj", ".dll", ".so", ".dylib", ".exe", ".bin", ".pdb",
    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".tiff", ".ico", ".mp3", ".mp4", ".m4a",
    ".mov", ".avi", ".mkv", ".webm", ".zip", ".tar", ".gz", ".7z", ".rar", ".pdf", ".doc",
    ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
];

/// Directory names whose contents we never descend. Mirrors Python
/// `_SKIP_DIR_NAMES`.
pub const SKIP_DIR_NAMES: &[&str] = &[
    "__pycache__",
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "venv",
    ".venv",
    "env",
    ".env",
    "dist",
    "build",
    "target",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    ".idea",
    ".vscode",
];

/// One chunk of file content with the metadata needed for delta passes
/// and search re-ranking. Mirrors the Python dict shape produced by
/// `_walk_and_chunk` + the LanceDB column set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexRow {
    /// Deterministic id derived from `(path, chunk_idx, mtime)`. Used as
    /// the key into [`VectorStore`].
    pub id: String,
    /// Absolute resolved path of the source file.
    pub path: String,
    /// 0-based chunk position within the file.
    pub chunk_idx: u32,
    /// Chunk text (utf-8). The vector store does NOT keep this; the
    /// sidecar JSON does.
    pub content: String,
    /// Source mtime at the time of indexing.
    pub mtime: f64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SidecarOnDisk {
    #[serde(default)]
    rows: Vec<IndexRow>,
}

/// Summary returned by [`index_full`] / [`index_delta`]. Mirrors the
/// fields Python's logger.info emits, plus an `embeddings_skipped`
/// flag for the degraded "embedder unreachable" path.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexStats {
    pub workspace_id: String,
    pub chunks_indexed: usize,
    pub files_removed: usize,
    pub files_present: usize,
    pub embeddings_skipped: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("unknown workspace {0:?}")]
    UnknownWorkspace(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

// ── Public entrypoints ───────────────────────────────────────────────

/// Indexing snapshot for a workspace. Mirrors Python `status()`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub id: String,
    pub exists: bool,
    pub path: Option<String>,
    pub file_count: u64,
    pub last_indexed_at: f64,
    pub last_activated_at: f64,
    pub indexing: bool,
}

pub fn status(workspace_id: &str) -> StatusReport {
    match get_workspace(workspace_id) {
        Some(w) => StatusReport {
            id: w.id.clone(),
            exists: true,
            path: Some(w.path.clone()),
            file_count: w.file_count,
            last_indexed_at: w.last_indexed_at,
            last_activated_at: w.last_activated_at,
            indexing: w.indexing,
        },
        None => StatusReport {
            id: workspace_id.to_owned(),
            exists: false,
            path: None,
            file_count: 0,
            last_indexed_at: 0.0,
            last_activated_at: 0.0,
            indexing: false,
        },
    }
}

/// Force a full reindex of `workspace_id`. The "Reindex" button.
pub async fn reindex_workspace(workspace_id: &str) -> Result<IndexStats, IndexError> {
    let w =
        get_workspace(workspace_id).ok_or_else(|| IndexError::UnknownWorkspace(workspace_id.to_owned()))?;
    index_full(&w).await
}

/// Delta refresh: only re-embed files changed since the last pass.
pub async fn refresh_workspace(workspace_id: &str) -> Result<IndexStats, IndexError> {
    let w =
        get_workspace(workspace_id).ok_or_else(|| IndexError::UnknownWorkspace(workspace_id.to_owned()))?;
    index_delta(&w).await
}

/// Drop every chunk and re-index from scratch.
pub async fn index_full(workspace: &Workspace) -> Result<IndexStats, IndexError> {
    set_indexing_flag(&workspace.id, true);
    let result = run_index_full(workspace).await;
    set_indexing_flag(&workspace.id, false);
    result
}

/// Re-index only files whose mtime is newer than the cached row, drop
/// rows whose backing file has disappeared.
pub async fn index_delta(workspace: &Workspace) -> Result<IndexStats, IndexError> {
    set_indexing_flag(&workspace.id, true);
    let result = run_index_delta(workspace).await;
    set_indexing_flag(&workspace.id, false);
    result
}

// ── Internal: index passes ───────────────────────────────────────────

async fn run_index_full(workspace: &Workspace) -> Result<IndexStats, IndexError> {
    let idx_dir = workspace_index_dir(&workspace.id)?;
    drop_existing_store(&idx_dir)?;

    let rows = walk_and_chunk(Path::new(&workspace.path));
    let unique_paths = count_unique_paths(&rows);

    let embeddings_skipped = embed_and_write(&idx_dir, &rows).await?;
    update_file_count(&workspace.id, unique_paths as u64);

    Ok(IndexStats {
        workspace_id: workspace.id.clone(),
        chunks_indexed: rows.len(),
        files_removed: 0,
        files_present: unique_paths,
        embeddings_skipped,
    })
}

async fn run_index_delta(workspace: &Workspace) -> Result<IndexStats, IndexError> {
    let idx_dir = workspace_index_dir(&workspace.id)?;
    let mut existing = load_sidecar(&idx_dir)?;

    // path → max mtime across that path's cached chunks.
    let mut cached_mtime: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for row in &existing.rows {
        cached_mtime
            .entry(row.path.clone())
            .and_modify(|prev| {
                if row.mtime > *prev {
                    *prev = row.mtime;
                }
            })
            .or_insert(row.mtime);
    }

    let walked = walk_and_chunk(Path::new(&workspace.path));
    let mut live_paths: HashSet<String> = HashSet::new();
    let mut new_rows: Vec<IndexRow> = Vec::new();
    for row in walked {
        live_paths.insert(row.path.clone());
        match cached_mtime.get(&row.path) {
            Some(prev) if *prev >= row.mtime - 0.001 => {} // unchanged, skip
            _ => new_rows.push(row),
        }
    }

    let gone: Vec<String> = cached_mtime
        .keys()
        .filter(|p| !live_paths.contains(*p))
        .cloned()
        .collect();

    // Build the post-delta row set: keep cached rows whose path is
    // still live AND wasn't re-chunked this pass; drop the rest.
    let changed_paths: HashSet<String> =
        new_rows.iter().map(|r| r.path.clone()).collect();
    existing.rows.retain(|r| {
        live_paths.contains(&r.path) && !changed_paths.contains(&r.path)
    });

    // Re-embed and merge in.
    let embeddings_skipped = if !new_rows.is_empty() {
        let skipped = embed_into_store(&idx_dir, &new_rows, /* clear = */ false).await?;
        existing.rows.extend(new_rows.iter().cloned());
        // Drop gone paths from the vector store too.
        if !gone.is_empty() {
            delete_paths_from_store(&idx_dir, &gone)?;
        }
        write_sidecar(&idx_dir, &existing)?;
        skipped
    } else {
        if !gone.is_empty() {
            delete_paths_from_store(&idx_dir, &gone)?;
        }
        write_sidecar(&idx_dir, &existing)?;
        false
    };

    update_file_count(&workspace.id, live_paths.len() as u64);

    Ok(IndexStats {
        workspace_id: workspace.id.clone(),
        chunks_indexed: existing.rows.len(),
        files_removed: gone.len(),
        files_present: live_paths.len(),
        embeddings_skipped,
    })
}

fn workspace_index_dir(workspace_id: &str) -> std::io::Result<PathBuf> {
    let dir = indexes_dir().join(workspace_id);
    ensure_dir(&dir)?;
    Ok(dir)
}

fn vector_path(idx_dir: &Path) -> PathBuf {
    idx_dir.join("files.bin")
}

fn sidecar_path(idx_dir: &Path) -> PathBuf {
    idx_dir.join("files.meta.json")
}

fn drop_existing_store(idx_dir: &Path) -> std::io::Result<()> {
    let v = vector_path(idx_dir);
    if v.exists() {
        std::fs::remove_file(v)?;
    }
    let m = sidecar_path(idx_dir);
    if m.exists() {
        std::fs::remove_file(m)?;
    }
    Ok(())
}

fn load_sidecar(idx_dir: &Path) -> Result<SidecarOnDisk, IndexError> {
    let path = sidecar_path(idx_dir);
    if !path.exists() {
        return Ok(SidecarOnDisk::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn write_sidecar(idx_dir: &Path, sc: &SidecarOnDisk) -> Result<(), IndexError> {
    let path = sidecar_path(idx_dir);
    let json = serde_json::to_string_pretty(sc)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Full-index variant: clear the store before writing. Returns
/// `embeddings_skipped = true` iff the embedder was unreachable AND
/// metadata-only rows were persisted (sidecar still updated).
async fn embed_and_write(idx_dir: &Path, rows: &[IndexRow]) -> Result<bool, IndexError> {
    if rows.is_empty() {
        write_sidecar(idx_dir, &SidecarOnDisk::default())?;
        return Ok(false);
    }
    let skipped = embed_into_store(idx_dir, rows, /* clear = */ true).await?;
    write_sidecar(
        idx_dir,
        &SidecarOnDisk {
            rows: rows.to_vec(),
        },
    )?;
    Ok(skipped)
}

async fn embed_into_store(
    idx_dir: &Path,
    rows: &[IndexRow],
    clear: bool,
) -> Result<bool, IndexError> {
    if rows.is_empty() {
        return Ok(false);
    }
    let dim = embed_dim();
    let path = vector_path(idx_dir);
    let mut store = if clear {
        VectorStore::new(dim)
    } else {
        VectorStore::load_or_empty(&path, dim)
    };

    let texts: Vec<String> = rows.iter().map(|r| r.content.clone()).collect();
    match embeddings::embed(texts).await {
        Ok(vectors) if vectors.len() == rows.len() => {
            for (row, vec) in rows.iter().zip(vectors.into_iter()) {
                if let Err(e) = store.insert(row.id.clone(), vec) {
                    tracing::warn!(
                        "workspaces.indexer: vector insert failed for {}: {e}",
                        row.id
                    );
                }
            }
            store.persist(&path).map_err(|e| match e {
                crate::memory::vector::VectorStoreError::Io(io) => IndexError::Io(io),
                other => IndexError::Io(std::io::Error::other(other.to_string())),
            })?;
            Ok(false)
        }
        Ok(vectors) => {
            tracing::warn!(
                "workspaces.indexer: embed returned {} vectors for {} rows — skipping vector write",
                vectors.len(),
                rows.len()
            );
            Ok(true)
        }
        Err(e) => {
            tracing::warn!("workspaces.indexer: embed failed: {e} — skipping vector write");
            Ok(true)
        }
    }
}

fn delete_paths_from_store(idx_dir: &Path, paths: &[String]) -> Result<(), IndexError> {
    let path = vector_path(idx_dir);
    let dim = embed_dim();
    let mut store = VectorStore::load_or_empty(&path, dim);
    let sc = load_sidecar(idx_dir)?;
    let gone: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut dirty = false;
    for row in &sc.rows {
        if gone.contains(row.path.as_str()) && store.delete(&row.id) {
            dirty = true;
        }
    }
    if dirty {
        store.persist(&path).map_err(|e| match e {
            crate::memory::vector::VectorStoreError::Io(io) => IndexError::Io(io),
            other => IndexError::Io(std::io::Error::other(other.to_string())),
        })?;
    }
    Ok(())
}

fn count_unique_paths(rows: &[IndexRow]) -> usize {
    rows.iter()
        .map(|r| r.path.as_str())
        .collect::<HashSet<&str>>()
        .len()
}

// ── Walk + chunk ─────────────────────────────────────────────────────

/// Walk `root` and yield every chunk of every indexable file. Pure
/// filesystem logic — no embedding, no IO beyond reads. Exposed for
/// test seams.
pub fn walk_and_chunk(root: &Path) -> Vec<IndexRow> {
    let mut out = Vec::new();
    walk_into(root, &mut out);
    out
}

fn walk_into(dir: &Path, out: &mut Vec<IndexRow>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(
                "workspaces.indexer: read_dir failed for {}: {e}",
                dir.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if SKIP_DIR_NAMES.contains(&name_str) {
                continue;
            }
            walk_into(&path, out);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        if is_skip_suffix(&path) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        if size == 0 || size > MAX_INDEXABLE_BYTES {
            if size > MAX_INDEXABLE_BYTES {
                tracing::debug!(
                    "workspaces.indexer: skip oversized {} ({} bytes)",
                    path.display(),
                    size
                );
            }
            continue;
        }
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    "workspaces.indexer: skip unreadable {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        if raw.iter().take(1024).any(|b| *b == 0) {
            continue; // binary sniff
        }
        let text = match std::str::from_utf8(&raw) {
            Ok(s) => s.to_owned(),
            Err(_) => String::from_utf8_lossy(&raw).into_owned(),
        };
        if text.trim().is_empty() {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let resolved = match path.canonicalize() {
            Ok(c) => c.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        };
        for (chunk_idx, chunk) in chunk_text(&text).into_iter().enumerate() {
            let id = row_id(&resolved, chunk_idx as u32, mtime);
            out.push(IndexRow {
                id,
                path: resolved.clone(),
                chunk_idx: chunk_idx as u32,
                content: chunk,
                mtime,
            });
        }
    }
}

fn is_skip_suffix(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    let dot_ext = format!(".{}", ext.to_ascii_lowercase());
    SKIP_SUFFIXES.iter().any(|s| *s == dot_ext)
}

/// Deterministic per-chunk id. Mirrors Python
/// `hashlib.sha256(f"{path}::{chunk_idx}::{mtime}").hexdigest()[:16]`.
pub fn row_id(path: &str, chunk_idx: u32, mtime: f64) -> String {
    // Python format string for the mtime is the float's `__str__`,
    // which for floats with a non-zero decimal part renders as e.g.
    // `1716480000.123456`. To stay byte-stable across the strangler
    // window we use `format!("{mtime}")` — Rust's `Display` for f64
    // produces the same shape as Python's `str(float)` for typical
    // values, and the id is opaque to callers anyway.
    let mut h = Sha256::new();
    h.update(format!("{path}::{chunk_idx}::{mtime}").as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..8])
}

/// Naive overlapping chunker. Pure function — exposed for tests.
/// `text` is treated as a sequence of Unicode codepoints (matches the
/// Python `text[start:start+N]` semantics).
pub fn chunk_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= CHUNK_SIZE_CHARS {
        return vec![text.to_owned()];
    }
    let step = CHUNK_SIZE_CHARS - CHUNK_OVERLAP_CHARS;
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = std::cmp::min(start + CHUNK_SIZE_CHARS, chars.len());
        out.push(chars[start..end].iter().collect::<String>());
        if end == chars.len() {
            break;
        }
        start += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(p: &Path, body: impl AsRef<[u8]>) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn chunk_text_short_input_returns_single_chunk_verbatim() {
        let chunks = chunk_text("hello world");
        assert_eq!(chunks, vec!["hello world".to_owned()]);
    }

    #[test]
    fn chunk_text_returns_overlapping_windows_for_long_input() {
        // 10_000 chars → step = 3800, so chunks at 0, 3800, 7600.
        let text: String = "a".repeat(10_000);
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chars().count(), CHUNK_SIZE_CHARS);
        assert_eq!(chunks[1].chars().count(), CHUNK_SIZE_CHARS);
        // Last chunk is the tail — 10_000 - 7600 = 2400 chars.
        assert_eq!(chunks[2].chars().count(), 2400);
    }

    #[test]
    fn chunk_text_respects_codepoint_boundaries_not_bytes() {
        // 5000 multi-byte chars; chunking on raw bytes would split mid-
        // codepoint and produce invalid UTF-8.
        let text: String = "日".repeat(5000);
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.chars().all(|ch| ch == '日'));
        }
    }

    #[test]
    fn walk_and_chunk_yields_one_row_per_chunk_with_metadata() {
        let td = tempdir().unwrap();
        let root = td.path();
        let p = root.join("a.txt");
        write(&p, "alpha");
        let rows = walk_and_chunk(root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "alpha");
        assert_eq!(rows[0].chunk_idx, 0);
        assert!(rows[0].path.ends_with("a.txt"));
        assert!(!rows[0].id.is_empty());
        assert!(rows[0].mtime > 0.0);
    }

    #[test]
    fn walk_and_chunk_skips_hidden_files_and_dirs() {
        let td = tempdir().unwrap();
        write(&td.path().join(".hidden.txt"), "should be skipped");
        write(&td.path().join(".secret/foo.txt"), "also skipped");
        write(&td.path().join("visible.txt"), "kept");
        let rows = walk_and_chunk(td.path());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path.ends_with("visible.txt"));
    }

    #[test]
    fn walk_and_chunk_skips_known_vcs_and_build_dirs() {
        let td = tempdir().unwrap();
        for dir in &["__pycache__", "node_modules", "target", ".git"] {
            write(&td.path().join(dir).join("inside.txt"), "skipped");
        }
        write(&td.path().join("src/main.txt"), "kept");
        let rows = walk_and_chunk(td.path());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path.ends_with("main.txt"));
    }

    #[test]
    fn walk_and_chunk_skips_known_binary_suffixes() {
        let td = tempdir().unwrap();
        write(&td.path().join("image.png"), "fake-png-bytes");
        write(&td.path().join("blob.zip"), "zip-bytes");
        write(&td.path().join("notes.md"), "kept");
        let rows = walk_and_chunk(td.path());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path.ends_with("notes.md"));
    }

    #[test]
    fn walk_and_chunk_skips_empty_and_whitespace_files() {
        let td = tempdir().unwrap();
        write(&td.path().join("empty.txt"), "");
        write(&td.path().join("whitespace.txt"), "   \n   \t");
        write(&td.path().join("real.txt"), "actual content");
        let rows = walk_and_chunk(td.path());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path.ends_with("real.txt"));
    }

    #[test]
    fn walk_and_chunk_skips_files_with_nul_byte_in_first_1kb() {
        let td = tempdir().unwrap();
        let mut bytes = vec![b'a'; 200];
        bytes.push(0);
        bytes.extend([b'b'; 50]);
        write(&td.path().join("binary.dat"), &bytes);
        let rows = walk_and_chunk(td.path());
        assert!(rows.is_empty());
    }

    #[test]
    fn walk_and_chunk_skips_files_above_size_cap() {
        let td = tempdir().unwrap();
        // (MAX_INDEXABLE_BYTES + 100) text bytes — just above the cap.
        let big: String = "x".repeat(MAX_INDEXABLE_BYTES as usize + 100);
        write(&td.path().join("huge.txt"), &big);
        let rows = walk_and_chunk(td.path());
        assert!(rows.is_empty());
    }

    #[test]
    fn walk_and_chunk_splits_long_files_into_multiple_chunks() {
        let td = tempdir().unwrap();
        let body: String = "z".repeat(9000);
        write(&td.path().join("long.txt"), &body);
        let rows = walk_and_chunk(td.path());
        assert!(rows.len() >= 2);
        assert_eq!(rows[0].chunk_idx, 0);
        assert_eq!(rows[1].chunk_idx, 1);
        // chunk_idx values are 0..N-1.
        let max_idx = rows.iter().map(|r| r.chunk_idx).max().unwrap();
        assert_eq!(max_idx as usize, rows.len() - 1);
    }

    #[test]
    fn walk_and_chunk_descends_into_nested_subdirs() {
        let td = tempdir().unwrap();
        write(&td.path().join("a.txt"), "1");
        write(&td.path().join("sub1/b.txt"), "2");
        write(&td.path().join("sub1/sub2/c.txt"), "3");
        let rows = walk_and_chunk(td.path());
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn walk_and_chunk_handles_utf8_content() {
        let td = tempdir().unwrap();
        write(&td.path().join("unicode.txt"), "日本語テスト".as_bytes());
        let rows = walk_and_chunk(td.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "日本語テスト");
    }

    #[test]
    fn row_id_is_deterministic_for_the_same_inputs() {
        let a = row_id("/foo/bar", 0, 1.5);
        let b = row_id("/foo/bar", 0, 1.5);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16); // sha256[..8] hex = 16 chars
    }

    #[test]
    fn row_id_differs_for_different_chunk_indexes() {
        let a = row_id("/foo/bar", 0, 1.5);
        let b = row_id("/foo/bar", 1, 1.5);
        assert_ne!(a, b);
    }

    #[test]
    fn row_id_differs_for_different_mtimes() {
        let a = row_id("/foo/bar", 0, 1.5);
        let b = row_id("/foo/bar", 0, 1.6);
        assert_ne!(a, b);
    }

    #[test]
    fn is_skip_suffix_is_case_insensitive() {
        assert!(is_skip_suffix(Path::new("a.PNG")));
        assert!(is_skip_suffix(Path::new("a.pNg")));
        assert!(is_skip_suffix(Path::new("a.png")));
        assert!(!is_skip_suffix(Path::new("a.txt")));
        assert!(!is_skip_suffix(Path::new("README")));
    }

    #[test]
    fn count_unique_paths_collapses_chunks_of_same_file() {
        let rows = vec![
            IndexRow {
                id: "a".into(),
                path: "/x".into(),
                chunk_idx: 0,
                content: "".into(),
                mtime: 0.0,
            },
            IndexRow {
                id: "b".into(),
                path: "/x".into(),
                chunk_idx: 1,
                content: "".into(),
                mtime: 0.0,
            },
            IndexRow {
                id: "c".into(),
                path: "/y".into(),
                chunk_idx: 0,
                content: "".into(),
                mtime: 0.0,
            },
        ];
        assert_eq!(count_unique_paths(&rows), 2);
    }

    #[test]
    fn write_sidecar_and_load_round_trip() {
        let td = tempdir().unwrap();
        let row = IndexRow {
            id: "rid".into(),
            path: "/abs/file".into(),
            chunk_idx: 0,
            content: "hello".into(),
            mtime: 1.5,
        };
        let sc = SidecarOnDisk {
            rows: vec![row.clone()],
        };
        write_sidecar(td.path(), &sc).unwrap();
        let back = load_sidecar(td.path()).unwrap();
        assert_eq!(back.rows.len(), 1);
        assert_eq!(back.rows[0], row);
    }
}
