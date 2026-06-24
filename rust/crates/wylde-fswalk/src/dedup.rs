//! Content-hash duplicate grouping — net-new detection logic.
//!
//! The workspaces manifest does *change-detection* (has this one path's content
//! drifted since last index?), never *dedup* (which distinct paths hold
//! identical bytes?). The organizer needs the latter to suggest removing
//! redundant copies, so this grouper is new here rather than extracted.
//!
//! Cheap-first: bucket candidates by **size** before hashing anything — two
//! files of different sizes can never be byte-identical, so a unique-size file
//! is never read. Only files that collide on size are hashed
//! ([`crate::stats::hash_file`]) and grouped by hash.

use std::collections::HashMap;

use crate::stats::{hash_file, walk_file_stats, FileStat};

/// A set of two-or-more paths whose file contents are byte-identical (same
/// size + same sha256-16 fingerprint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// The shared 16-hex-char content hash.
    pub hash: String,
    /// The shared byte size.
    pub size: u64,
    /// The canonical paths sharing this content, in discovery order. Always
    /// length ≥ 2 (singletons are not duplicate groups).
    pub paths: Vec<String>,
}

impl DuplicateGroup {
    /// Bytes that could be reclaimed by keeping one copy and removing the rest:
    /// `size * (count - 1)`.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.size.saturating_mul((self.paths.len() as u64).saturating_sub(1))
    }
}

/// Group `files` (already-walked [`FileStat`]s) into duplicate sets by
/// identical content. Size is the cheap pre-filter: only files that share a
/// size with at least one other are hashed; everything else is skipped without
/// a read. Returns only groups of length ≥ 2, sorted by reclaimable bytes
/// descending so the biggest wins surface first.
pub fn group_file_stats(files: &[FileStat]) -> Vec<DuplicateGroup> {
    // Pass 1 — bucket by size (no IO).
    let mut by_size: HashMap<u64, Vec<&FileStat>> = HashMap::new();
    for f in files {
        by_size.entry(f.size).or_default().push(f);
    }

    // Pass 2 — within each size-collision bucket, hash + bucket by content.
    // `(size, hash)` keys keep groups from different sizes apart even on the
    // astronomically unlikely truncated-hash collision across sizes.
    let mut by_hash: HashMap<(u64, String), Vec<String>> = HashMap::new();
    let mut order: Vec<(u64, String)> = Vec::new();
    for (size, bucket) in by_size {
        if bucket.len() < 2 {
            continue; // unique size — cannot have a duplicate, never read it.
        }
        for f in bucket {
            let Some((hash, _, _)) = hash_file(&f.path) else {
                // Unreadable now (deleted / locked) — drop from dedup, not fatal.
                continue;
            };
            let key = (size, hash);
            let entry = by_hash.entry(key.clone()).or_default();
            if entry.is_empty() {
                order.push(key);
            }
            entry.push(f.path.clone());
        }
    }

    let mut groups: Vec<DuplicateGroup> = order
        .into_iter()
        .filter_map(|key| {
            let paths = by_hash.remove(&key)?;
            if paths.len() < 2 {
                return None;
            }
            let (size, hash) = key;
            Some(DuplicateGroup { hash, size, paths })
        })
        .collect();
    groups.sort_by(|a, b| b.reclaimable_bytes().cmp(&a.reclaimable_bytes()));
    groups
}

/// Group an explicit list of paths into duplicate sets. Convenience over
/// [`group_file_stats`] for callers that already hold paths but not stats —
/// each path is `stat`'d for its size first.
pub fn group_duplicates(paths: &[String]) -> Vec<DuplicateGroup> {
    let stats: Vec<FileStat> = paths
        .iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(FileStat {
                path: p.clone(),
                mtime: crate::stats::mtime_secs(&meta),
                size: meta.len(),
            })
        })
        .collect();
    group_file_stats(&stats)
}

/// Walk `root` (respecting the [`ExclusionMatcher`](crate::ExclusionMatcher) +
/// binary-suffix guard via [`walk_file_stats`]) and return its duplicate
/// groups. The one-call "find redundant copies under here" entry point.
pub fn find_duplicates_under(root: &str) -> Vec<DuplicateGroup> {
    group_file_stats(&walk_file_stats(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finds_identical_files_and_ignores_uniques() {
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.txt"), "duplicated content here").unwrap();
        std::fs::write(root.join("b.txt"), "duplicated content here").unwrap();
        std::fs::write(root.join("c.txt"), "totally unique content!!").unwrap();
        let groups = find_duplicates_under(&root.to_string_lossy());
        assert_eq!(groups.len(), 1, "exactly one duplicate group");
        assert_eq!(groups[0].paths.len(), 2);
        assert!(groups[0].reclaimable_bytes() > 0);
    }

    #[test]
    fn same_size_different_content_is_not_a_duplicate() {
        let td = tempdir().unwrap();
        let root = td.path();
        // Both 8 bytes, different content — collide on size, differ on hash.
        std::fs::write(root.join("a.bin"), "AAAAAAAA").unwrap();
        std::fs::write(root.join("b.bin"), "BBBBBBBB").unwrap();
        let groups = find_duplicates_under(&root.to_string_lossy());
        assert!(groups.is_empty(), "same size but different bytes is not a dup");
    }

    #[test]
    fn three_way_duplicate_reclaims_two_copies() {
        let td = tempdir().unwrap();
        let root = td.path();
        for n in ["x1.txt", "x2.txt", "x3.txt"] {
            std::fs::write(root.join(n), "three copies of me").unwrap();
        }
        let groups = find_duplicates_under(&root.to_string_lossy());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 3);
        let one = "three copies of me".len() as u64;
        assert_eq!(groups[0].reclaimable_bytes(), one * 2);
    }

    #[test]
    fn group_duplicates_over_explicit_paths() {
        let td = tempdir().unwrap();
        let root = td.path();
        let a = root.join("a");
        let b = root.join("b");
        std::fs::write(&a, "same").unwrap();
        std::fs::write(&b, "same").unwrap();
        let paths = vec![a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()];
        let groups = group_duplicates(&paths);
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn empty_input_is_no_groups() {
        assert!(group_duplicates(&[]).is_empty());
        let td = tempdir().unwrap();
        assert!(find_duplicates_under(&td.path().to_string_lossy()).is_empty());
    }
}
