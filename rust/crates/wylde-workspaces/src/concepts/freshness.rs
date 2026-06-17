//! Concept freshness / drift detection (TBS concept-system Phase 4, thesis §6
//! risk 4 / §7 S4.3).
//!
//! Code changes; concepts go stale (members edited, files deleted, new
//! subsystems unlabeled). Rather than silently presenting a stale concept as
//! authoritative (the honesty rule the index-staleness work enforced), we
//! surface a freshness signal: tie a concept's build time (`updated_at`) to its
//! member files' latest chunk mtimes (the same per-file mtime the delta-indexer
//! and slice-2.5 active-file boost already track).
//!
//! A concept is **stale** when a member file changed *after* the concept was
//! built (churned) or vanished from the index (a member file no longer has any
//! chunk). Pure + unit-tested; the verb assembles the per-file mtime map from
//! the chunk store.

use std::collections::HashMap;

use serde::Serialize;

use super::concept::Concept;

/// Freshness verdict for one concept.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConceptFreshness {
    pub id: String,
    /// Stale = at least one member file churned-since-build or went missing.
    pub stale: bool,
    /// Member files whose latest chunk mtime is newer than the build.
    pub churned_files: Vec<String>,
    /// Member files no longer present in the index (deleted/renamed away).
    pub missing_files: Vec<String>,
    /// When the concept was last (re)built (`Concept::updated_at`).
    pub built_at: f64,
    /// The newest member-file mtime observed (0 if none resolved).
    pub newest_member_mtime: f64,
}

/// Assess one concept against a `file → latest mtime` map (built from the chunk
/// store). A small epsilon guards float equality so a file rewritten in the
/// same build pass doesn't read as churned.
pub fn assess(concept: &Concept, file_mtimes: &HashMap<String, f64>) -> ConceptFreshness {
    const EPS: f64 = 1.0; // 1s — mtime resolution slack
    let mut churned = Vec::new();
    let mut missing = Vec::new();
    let mut newest = 0.0f64;
    for f in &concept.member_files {
        match file_mtimes.get(f) {
            Some(&m) => {
                if m > newest {
                    newest = m;
                }
                if m > concept.updated_at + EPS {
                    churned.push(f.clone());
                }
            }
            None => missing.push(f.clone()),
        }
    }
    churned.sort();
    missing.sort();
    ConceptFreshness {
        id: concept.id.clone(),
        stale: !churned.is_empty() || !missing.is_empty(),
        churned_files: churned,
        missing_files: missing,
        built_at: concept.updated_at,
        newest_member_mtime: newest,
    }
}

/// Build the `file → latest mtime` map from a chunk list (max mtime per path).
pub fn file_mtimes_from_chunks(
    chunks: &[crate::rag::indexer::store::IndexedChunk],
) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    for c in chunks {
        let e = out.entry(c.path.clone()).or_insert(0.0);
        if c.mtime > *e {
            *e = c.mtime;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::ConceptSource;

    fn concept(files: &[&str], built_at: f64) -> Concept {
        let mut c = Concept::new("c1", "C", "d", ConceptSource::Embedding);
        c.member_files = files.iter().map(|s| s.to_string()).collect();
        c.updated_at = built_at;
        c
    }

    #[test]
    fn fresh_when_no_file_changed_since_build() {
        let c = concept(&["a.rs", "b.rs"], 100.0);
        let mtimes = HashMap::from([("a.rs".to_owned(), 90.0), ("b.rs".to_owned(), 50.0)]);
        let f = assess(&c, &mtimes);
        assert!(!f.stale);
        assert!(f.churned_files.is_empty() && f.missing_files.is_empty());
        assert_eq!(f.newest_member_mtime, 90.0);
    }

    #[test]
    fn stale_when_a_member_file_churned() {
        let c = concept(&["a.rs", "b.rs"], 100.0);
        let mtimes = HashMap::from([("a.rs".to_owned(), 150.0), ("b.rs".to_owned(), 50.0)]);
        let f = assess(&c, &mtimes);
        assert!(f.stale);
        assert_eq!(f.churned_files, vec!["a.rs"]);
    }

    #[test]
    fn stale_when_a_member_file_went_missing() {
        let c = concept(&["a.rs", "gone.rs"], 100.0);
        let mtimes = HashMap::from([("a.rs".to_owned(), 50.0)]);
        let f = assess(&c, &mtimes);
        assert!(f.stale);
        assert_eq!(f.missing_files, vec!["gone.rs"]);
    }

    #[test]
    fn epsilon_tolerates_same_build_rewrite() {
        let c = concept(&["a.rs"], 100.0);
        // mtime exactly at build time (± <1s) is not churned.
        let mtimes = HashMap::from([("a.rs".to_owned(), 100.5)]);
        assert!(!assess(&c, &mtimes).stale);
    }

    #[test]
    fn mtime_map_takes_max_per_path() {
        use crate::rag::indexer::store::IndexedChunk;
        let mk = |path: &str, idx: u32, mtime: f64| IndexedChunk {
            id: format!("{path}:{idx}"),
            path: path.to_owned(),
            chunk_idx: idx,
            content: String::new(),
            mtime,
            start_line: 1,
            end_line: 1,
            vector: vec![],
        };
        let chunks = vec![mk("a.rs", 0, 10.0), mk("a.rs", 1, 30.0), mk("b.rs", 0, 5.0)];
        let m = file_mtimes_from_chunks(&chunks);
        assert_eq!(m["a.rs"], 30.0);
        assert_eq!(m["b.rs"], 5.0);
    }
}
