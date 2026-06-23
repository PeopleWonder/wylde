//! Metadata-only walking, content-fingerprinting, and path pre-filtering —
//! the detection primitives shared by the RAG indexer's delta/manifest passes
//! and the `wylde-organize` scanner.
//!
//! Everything here is read-only and pure over a path. No chunking, no
//! embedding, no storage — those stay in the consumer (`wylde-workspaces`'s
//! `rag/indexer/walk.rs` keeps the chunker and imports the helpers below).

use std::path::Path;

use crate::exclude::ExclusionMatcher;

/// Files we never try to read — bytecode caches, VCS metadata, and the
/// obvious binary-blob extensions a binary-sniff would catch anyway.
/// Matched case-insensitively on the file's own suffix. This is a
/// file-*content* guard (distinct from the path/dir [`ExclusionMatcher`]).
pub const SKIP_SUFFIXES: &[&str] = &[
    "pyc", "pyo", "class", "o", "obj", "dll", "so", "dylib", "exe", "bin", "pdb", "jpg", "jpeg",
    "png", "gif", "bmp", "webp", "tiff", "ico", "mp3", "mp4", "m4a", "mov", "avi", "mkv", "webm",
    "zip", "tar", "gz", "7z", "rar", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
];

/// The canonical, absolute string form a file's path is stored under, so two
/// independent walks (or a walk and a later delete) agree on the same key.
/// Tolerant of a missing file — on a delete the file is already gone, so it
/// canonicalises the parent dir and re-joins the name (the parent is normally
/// still present), giving the same string the walk produced for that file while
/// it existed. On Windows this carries the `\\?\` extended-length prefix; all
/// producers use this one helper, so they agree.
pub fn canonical_path(path: &Path) -> String {
    if let Ok(c) = path.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = parent.canonicalize() {
            return cp.join(name).to_string_lossy().into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

/// Path-only pre-filter applied before any IO: would the full walk have indexed
/// a file at `path` under `root`? Delegates to the same [`ExclusionMatcher`]
/// the walk consults (dotfiles, skip-dirs, the `target-*` build trees, nested
/// `.gitignore` / `.wyldeignore`), plus the binary-suffix guard
/// ([`SKIP_SUFFIXES`]) kept here as a file-content concern.
///
/// Because it shares the matcher with the walk, the watcher and the walk agree
/// byte-for-byte on what's indexable, so a `target-dev/`, `.git/`,
/// `node_modules/`, hidden, or binary-suffixed path never triggers a delta.
pub fn is_indexable_path(root: &str, path: &str) -> bool {
    let p = Path::new(path);
    // Binary-suffix reject stays here — it's a file-content guard the
    // exclusion matcher (a path/dir concern) deliberately doesn't own.
    if let Some(suffix) = p.extension().and_then(|s| s.to_str()) {
        if SKIP_SUFFIXES.contains(&suffix.to_ascii_lowercase().as_str()) {
            return false;
        }
    }
    // The same shared predicate the full walk uses — so the watcher and the
    // walk agree byte-for-byte on what's indexable. Built fresh per call
    // (cheap, lazy) so it always reflects the current `.gitignore` files.
    !ExclusionMatcher::for_root(Path::new(root)).is_excluded(p, false)
}

/// Read-only dry-run of the exclusion over a folder. Counts files the walk
/// *would* index vs exclude and samples some excluded paths — so the matcher's
/// effect (e.g. the `target-dev/doc` rustdoc tree dropping out) can be confirmed
/// before a purge.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WalkPreview {
    /// Files that would be indexed.
    pub would_index: u32,
    /// Files that would be excluded.
    pub would_exclude: u32,
    /// Up to `sample_cap` excluded file paths, for eyeballing.
    pub sample_excluded: Vec<String>,
}

/// Walk `folder` read-only and classify each file via the [`ExclusionMatcher`].
/// Descends into excluded dirs (e.g. `target-dev`) so their files are *counted*
/// as excluded — except `.git`, pruned as pure noise. No embed, no persist.
pub fn walk_preview(folder: &str, sample_cap: usize) -> WalkPreview {
    let mut pv = WalkPreview::default();
    let root = Path::new(folder);
    if !root.is_dir() {
        return pv;
    }
    let matcher = ExclusionMatcher::for_root(root);
    preview_dir(root, &matcher, sample_cap, &mut pv);
    pv
}

fn preview_dir(dir: &Path, matcher: &ExclusionMatcher, cap: usize, pv: &mut WalkPreview) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            // Prune `.git` descent — thousands of git objects would swamp the
            // counts with noise no walk would ever index anyway.
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            preview_dir(&path, matcher, cap, pv);
        } else if file_type.is_file() {
            if matcher.is_excluded(&path, false) {
                pv.would_exclude += 1;
                if pv.sample_excluded.len() < cap {
                    pv.sample_excluded.push(path.to_string_lossy().into_owned());
                }
            } else {
                pv.would_index += 1;
            }
        }
    }
}

/// Metadata-only view of one file — the cheap (path, mtime, size) triple a
/// content-hash manifest diff walks **without reading any file content**. The
/// `(mtime, size)` fast-path lets an unchanged file skip both the read AND the
/// hash; only files whose `(mtime, size)` drifted are read + hashed to confirm
/// a real change. **mtime only — no atime** (atime is unreliable: Windows
/// last-access is often disabled, Linux uses `relatime`).
#[derive(Clone, Debug, PartialEq)]
pub struct FileStat {
    /// Canonical, absolute path — the same key the chunk store + manifest use.
    pub path: String,
    /// Source-file mtime (epoch seconds).
    pub mtime: f64,
    /// Source-file size in bytes.
    pub size: u64,
}

/// Metadata-only walk of `folder`: every file the full walk *would* index,
/// as a [`FileStat`] (no content read). Shares the one [`ExclusionMatcher`] +
/// the binary-suffix guard with the chunker so a manifest diff sees exactly the
/// set the chunker does. Content-level skips (binary sniff, empty, oversize)
/// are deferred to the chunker when a changed file is actually re-chunked — a
/// metadata walk can't know them, and a file that turns out unchunkable simply
/// yields zero chunks (treated as a removal).
pub fn walk_file_stats(folder: &str) -> Vec<FileStat> {
    let mut out = Vec::new();
    let root = Path::new(folder);
    if !root.is_dir() {
        return out;
    }
    let matcher = ExclusionMatcher::for_root(root);
    stat_dir(root, &matcher, &mut out);
    out
}

fn stat_dir(dir: &Path, matcher: &ExclusionMatcher, out: &mut Vec<FileStat>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let is_dir = file_type.is_dir();
        if matcher.is_excluded(&path, is_dir) {
            continue;
        }
        if is_dir {
            stat_dir(&path, matcher, out);
        } else if file_type.is_file() {
            // Binary-suffix reject mirrors the chunker / `is_indexable_path`.
            if let Some(suffix) = path.extension().and_then(|s| s.to_str()) {
                if SKIP_SUFFIXES.contains(&suffix.to_ascii_lowercase().as_str()) {
                    continue;
                }
            }
            let Ok(meta) = entry.metadata() else { continue };
            out.push(FileStat {
                path: canonical_path(&path),
                mtime: mtime_secs(&meta),
                size: meta.len(),
            });
        }
    }
}

/// Read `path` and return its content hash + `(size, mtime)` — the per-file
/// fingerprint. The hash is `sha256(file bytes)` truncated to 16 hex chars
/// (same discipline as the per-chunk id), so a `touch`/checkout that changes
/// mtime but not bytes hashes identically and avoids a re-embed. `None` on an
/// unreadable file (caller keeps the prior entry).
pub fn hash_file(path: &str) -> Option<(String, u64, f64)> {
    use sha2::{Digest, Sha256};
    let p = Path::new(path);
    let meta = p.metadata().ok()?;
    let bytes = std::fs::read(p).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hex::encode(hasher.finalize())[..16].to_owned();
    Some((hash, meta.len(), mtime_secs(&meta)))
}

/// Source-file mtime as epoch seconds (`f64`). Falls back to `0.0` if the
/// platform can't report it.
pub fn mtime_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn is_indexable_path_accepts_normal_source() {
        assert!(is_indexable_path("/proj", "/proj/src/main.rs"));
        assert!(is_indexable_path("/proj", "/proj/docs/readme.md"));
    }

    #[test]
    fn is_indexable_path_rejects_skip_dirs_and_hidden_anywhere() {
        // A skip-dir anywhere in the ancestry under root.
        assert!(!is_indexable_path("/proj", "/proj/target/debug/foo.rs"));
        assert!(!is_indexable_path("/proj", "/proj/node_modules/dep/x.js"));
        assert!(!is_indexable_path("/proj", "/proj/.git/config"));
        // A hidden file or hidden dir component.
        assert!(!is_indexable_path("/proj", "/proj/.env"));
        assert!(!is_indexable_path("/proj", "/proj/.vscode/settings.json"));
        assert!(!is_indexable_path("/proj", "/proj/src/.secret.rs"));
    }

    #[test]
    fn is_indexable_path_rejects_binary_suffixes() {
        assert!(!is_indexable_path("/proj", "/proj/assets/logo.png"));
        assert!(!is_indexable_path("/proj", "/proj/bin/tool.exe"));
        assert!(!is_indexable_path("/proj", "/proj/lib/native.dll"));
    }

    #[test]
    fn canonical_path_is_stable_across_existing_and_deleted() {
        let td = tempdir().unwrap();
        let f = td.path().join("file.rs");
        std::fs::write(&f, "x").unwrap();
        let while_present = canonical_path(&f);
        std::fs::remove_file(&f).unwrap();
        let after_delete = canonical_path(&f);
        // The lenient (parent + name) form after deletion matches the form
        // produced while the file existed.
        assert_eq!(while_present, after_delete);
    }

    #[test]
    fn walk_file_stats_skips_excluded_and_binary() {
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("good.md"), "# Title\nsome prose").unwrap();
        std::fs::write(root.join("skipme.png"), "not really png but suffix").unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules").join("dep.md"), "dep").unwrap();
        let stats = walk_file_stats(&root.to_string_lossy());
        assert_eq!(stats.len(), 1, "only good.md should be stat'd");
        assert!(stats[0].path.ends_with("good.md") || stats[0].path.contains("good.md"));
        assert!(stats[0].size > 0);
    }

    #[test]
    fn hash_file_is_content_addressed_not_mtime() {
        let td = tempdir().unwrap();
        let a = td.path().join("a.txt");
        let b = td.path().join("b.txt");
        std::fs::write(&a, "identical bytes").unwrap();
        std::fs::write(&b, "identical bytes").unwrap();
        let (ha, _, _) = hash_file(&a.to_string_lossy()).unwrap();
        let (hb, _, _) = hash_file(&b.to_string_lossy()).unwrap();
        assert_eq!(ha, hb, "same content hashes the same regardless of path/mtime");
        assert_eq!(ha.len(), 16, "16-hex-char truncation");
        std::fs::write(&b, "different bytes!").unwrap();
        let (hb2, _, _) = hash_file(&b.to_string_lossy()).unwrap();
        assert_ne!(ha, hb2);
    }

    #[test]
    fn walk_preview_counts_indexed_vs_excluded() {
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("keep.rs"), "fn main(){}").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target").join("out.rs"), "artifact").unwrap();
        let pv = walk_preview(&root.to_string_lossy(), 10);
        assert_eq!(pv.would_index, 1);
        assert_eq!(pv.would_exclude, 1);
    }
}
