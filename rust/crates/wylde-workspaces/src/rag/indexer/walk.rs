//! Walk a workspace folder and split text-bearing files into overlapping
//! chunks. Rust port of the retired Python `_index.py::_walk_and_chunk`
//! / `_chunk_text`.
//!
//! Binary / oversized / VCS / venv files are skipped on a best-effort
//! basis so the indexer never blocks on a multi-MB blob or feeds a
//! non-text file to the embedder.
//!
//! ## Detection moved to `wylde-fswalk`
//!
//! The path-exclusion predicate ([`ExclusionMatcher`]), the metadata-only walk
//! ([`walk_file_stats`] / [`FileStat`]), the per-file content fingerprint
//! ([`hash_file`]), the canonical-path helper ([`canonical_path`]), the path
//! pre-filter ([`is_indexable_path`]), the binary-suffix guard
//! ([`SKIP_SUFFIXES`]), and the read-only preview ([`walk_preview`]) were
//! extracted into the shared [`wylde_fswalk`] crate (file-organizer build,
//! 2026-06-23) so the `wylde-organize` Service reuses the identical detectors.
//! They are re-exported below so every existing `walk::<name>` reference across
//! the indexer keeps resolving unchanged. Only the **chunker** — the RAG-
//! specific text-splitting the embedder needs — stays here.

use std::path::Path;

use super::exclude::ExclusionMatcher;

// The detection primitives that moved to `wylde-fswalk`. Re-exported so the
// indexer's `walk::canonical_path` / `walk::walk_file_stats` / `walk::FileStat`
// / `walk::hash_file` / `walk::is_indexable_path` / `walk::walk_preview` /
// `walk::WalkPreview` / `walk::SKIP_SUFFIXES` references stay valid after the
// extraction (pure move + dep-swap).
pub use wylde_fswalk::{
    canonical_path, hash_file, is_indexable_path, mtime_secs, walk_file_stats, walk_preview,
    FileStat, WalkPreview, SKIP_SUFFIXES,
};

/// Soft-cap text-file size at 1 MB. Bigger files are logged-and-skipped
/// rather than crashing the embedder with a multi-MB chunk.
const MAX_INDEXABLE_BYTES: u64 = 1024 * 1024;
/// Chunk boundary for long files (chars). Embedders cap at ~512–1024
/// tokens; 4 KB of text is comfortably under that for english-ish content.
const CHUNK_SIZE_CHARS: usize = 4000;
/// Overlap between adjacent chunks so the embedder sees context spanning
/// a chunk boundary.
const CHUNK_OVERLAP_CHARS: usize = 200;

/// One indexable chunk before embedding: where it came from + the text.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// Absolute path to the source file.
    pub path: String,
    /// 0-based index of this chunk within its file.
    pub chunk_idx: u32,
    /// The chunk text.
    pub content: String,
    /// Source file mtime (epoch seconds) at walk time.
    pub mtime: f64,
    /// 1-based first line of this chunk in its source file.
    pub start_line: u32,
    /// 1-based last line of this chunk in its source file.
    pub end_line: u32,
}

/// Walk `folder` and yield every indexable chunk under it. Skips binary /
/// oversized / hidden / VCS / venv files and directories on a best-effort
/// basis. Never panics — unreadable entries are skipped with a debug log.
pub fn walk_and_chunk(folder: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let root = Path::new(folder);
    if !root.is_dir() {
        return out;
    }
    // One matcher for the whole walk — its per-directory `.gitignore` /
    // `.wyldeignore` matchers are built lazily + cached, so each ignore file is
    // read at most once across the descent.
    let matcher = ExclusionMatcher::for_root(root);
    walk_dir(root, &matcher, &mut out);
    out
}

fn walk_dir(dir: &Path, matcher: &ExclusionMatcher, out: &mut Vec<Chunk>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("workspaces.rag: skip unreadable dir {dir:?}: {e}");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let is_dir = file_type.is_dir();
        // The one shared predicate: dotfiles, skip-dirs, the `target-*` build
        // trees, and anything a nested `.gitignore` / `.wyldeignore` excludes.
        if matcher.is_excluded(&path, is_dir) {
            continue;
        }
        if is_dir {
            walk_dir(&path, matcher, out);
        } else if file_type.is_file() {
            out.extend(chunk_file(&path));
        }
    }
}

/// Chunk one on-disk file, applying the same suffix / size / binary / empty
/// skips the full walk does. Returns the file's chunks, or an empty vec when
/// the file is filtered out or unreadable. This is the single-file entry point
/// the watcher's delta path drives (Slice I); the full walk recurses into it.
///
/// The caller is responsible for the path-level prune (skip-dirs / hidden /
/// suffix) via [`is_indexable_path`] — this function still re-checks the suffix
/// and content so a direct call is self-contained.
pub fn chunk_one_file(path: &str) -> Vec<Chunk> {
    chunk_file(Path::new(path))
}

fn chunk_file(path: &Path) -> Vec<Chunk> {
    let mut out = Vec::new();
    if let Some(suffix) = path.extension().and_then(|s| s.to_str()) {
        if SKIP_SUFFIXES.contains(&suffix.to_ascii_lowercase().as_str()) {
            return out;
        }
    }
    let meta = match path.metadata() {
        Ok(m) => m,
        Err(_) => return out,
    };
    let size = meta.len();
    if size == 0 {
        return out;
    }
    if size > MAX_INDEXABLE_BYTES {
        tracing::debug!("workspaces.rag: skip oversized {path:?} ({size} bytes)");
        return out;
    }
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("workspaces.rag: skip unreadable {path:?}: {e}");
            return out;
        }
    };
    // Binary sniff — a NUL byte in the first 1 KB is a strong signal of a
    // non-text file the embedder shouldn't see.
    if raw.iter().take(1024).any(|b| *b == 0) {
        return out;
    }
    let text = String::from_utf8_lossy(&raw);
    if text.trim().is_empty() {
        return out;
    }
    let mtime = mtime_secs(&meta);
    // Absolute, canonicalised path so delta lookups match across walks.
    let abs = canonical_path(path);
    for (chunk_idx, (content, start_line, end_line)) in chunk_text(&text).into_iter().enumerate() {
        out.push(Chunk {
            path: abs.clone(),
            chunk_idx: chunk_idx as u32,
            content,
            mtime,
            start_line,
            end_line,
        });
    }
    out
}

/// Naive overlapping chunker with line-range tracking. Short files become
/// a single chunk; long files get a few overlapping windows so the
/// embedder sees context spanning section boundaries.
///
/// Returns `(content, start_line, end_line)` with **1-based** inclusive
/// line numbers, computed over the original text.
pub fn chunk_text(text: &str) -> Vec<(String, u32, u32)> {
    let chars: Vec<char> = text.chars().collect();
    // Prefix newline count: nl_prefix[i] = count of '\n' in chars[0..i].
    let mut nl_prefix = Vec::with_capacity(chars.len() + 1);
    let mut running = 0u32;
    nl_prefix.push(0u32);
    for &c in &chars {
        if c == '\n' {
            running += 1;
        }
        nl_prefix.push(running);
    }
    let line_at = |idx: usize| -> u32 { nl_prefix[idx.min(chars.len())] + 1 };

    if chars.len() <= CHUNK_SIZE_CHARS {
        let end_line = if chars.is_empty() {
            1
        } else {
            line_at(chars.len() - 1)
        };
        return vec![(text.to_owned(), 1, end_line)];
    }

    let step = CHUNK_SIZE_CHARS - CHUNK_OVERLAP_CHARS;
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + CHUNK_SIZE_CHARS).min(chars.len());
        let content: String = chars[start..end].iter().collect();
        let start_line = line_at(start);
        let end_line = line_at(end - 1);
        chunks.push((content, start_line, end_line));
        start += step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn chunk_text_short_is_single_chunk_with_full_line_span() {
        let chunks = chunk_text("line one\nline two\nline three");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, 1);
        assert_eq!(chunks[0].2, 3);
        assert_eq!(chunks[0].0, "line one\nline two\nline three");
    }

    #[test]
    fn chunk_text_long_overlaps_and_tracks_lines() {
        // 10k chars across many lines forces multiple windows.
        let line = "abcdefghij\n"; // 11 chars incl newline
        let text = line.repeat(1000); // 11_000 chars, 1000 lines
        let chunks = chunk_text(&text);
        assert!(
            chunks.len() >= 3,
            "expected multiple chunks, got {}",
            chunks.len()
        );
        // First chunk starts at line 1.
        assert_eq!(chunks[0].1, 1);
        // Windows advance (overlap < size) so successive starts increase.
        assert!(chunks[1].1 > chunks[0].1);
        // Last chunk reaches the final line.
        assert_eq!(chunks.last().unwrap().2, 1000);
    }

    #[test]
    fn walk_skips_binary_oversized_hidden_and_vcs() {
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("good.md"), "# Title\nsome prose").unwrap();
        std::fs::write(root.join("binary.dat"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(root.join("skipme.png"), "not really png but suffix").unwrap();
        std::fs::write(root.join(".hidden.md"), "hidden").unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("config.md"), "vcs").unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules").join("dep.md"), "dep").unwrap();

        let chunks = walk_and_chunk(&root.to_string_lossy());
        assert_eq!(chunks.len(), 1, "only good.md should be indexed");
        assert!(chunks[0].content.contains("some prose"));
    }

    #[test]
    fn walk_recurses_into_normal_subdirs() {
        let td = tempdir().unwrap();
        let root = td.path();
        let sub = root.join("docs");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "alpha").unwrap();
        std::fs::write(root.join("b.rs"), "fn main() {}").unwrap();
        let chunks = walk_and_chunk(&root.to_string_lossy());
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn walk_of_missing_folder_is_empty() {
        assert!(walk_and_chunk("/no/such/folder/xyz-123").is_empty());
    }

    #[test]
    fn chunk_one_file_skips_binary_and_empty_returns_chunks_for_text() {
        let td = tempdir().unwrap();
        let root = td.path();
        let good = root.join("good.rs");
        std::fs::write(&good, "fn foo() -> i32 { 42 }\n").unwrap();
        let binary = root.join("blob.bin");
        std::fs::write(&binary, [0u8, 1, 2, 3, 0, 9]).unwrap();
        let empty = root.join("empty.md");
        std::fs::write(&empty, "").unwrap();

        let chunks = chunk_one_file(&good.to_string_lossy());
        assert_eq!(chunks.len(), 1, "text file yields one chunk");
        assert!(chunks[0].content.contains("foo"));

        // Binary suffix is skipped by suffix; a NUL-bearing non-suffixed file
        // is skipped by the content sniff.
        assert!(chunk_one_file(&binary.to_string_lossy()).is_empty());
        let nul = root.join("weird.txt");
        std::fs::write(&nul, [b'a', 0u8, b'b']).unwrap();
        assert!(chunk_one_file(&nul.to_string_lossy()).is_empty());
        assert!(chunk_one_file(&empty.to_string_lossy()).is_empty());
    }
}
