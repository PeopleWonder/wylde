//! Concept-driven retrieval (TBS concept-system Phase 3, thesis §3.3 S3.2) —
//! the concept as the **RAG unit**.
//!
//! Today's RAG injects the 5 nearest *chunks* to the raw query. The concept
//! system injects a *coherent member set*: given an (activated) concept, select
//! representative chunks from its members — ranked by cosine to the concept
//! **centroid**, then MMR-diversified so the injection isn't five near-
//! duplicates (reusing the slice-2.1 diversity idea). This is the retrieval
//! *mechanism*; **which** concept(s) to activate per query (routing, §3.4) is
//! the explicitly-deferred next phase — this primitive runs on a concept the
//! caller chose (via the browse "activate"/lens, or, later, the router).
//!
//! Pure + unit-tested. The injection composes with the §3.1 lens: pass the
//! lens-restricted file set as `allowed_files` to inject "concept ∩ scope."

use std::collections::HashSet;

use serde::Serialize;

use crate::rag::indexer::store::IndexedChunk;

/// MMR relevance/diversity trade-off (matches the RAG indexer's `MMR_LAMBDA`).
pub const MMR_LAMBDA: f32 = 0.7;

/// One representative chunk selected for injection.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConceptSnippet {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    /// Cosine to the concept centroid (the true relevance, not the MMR score).
    pub score: f32,
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Select up to `k` representative chunks for a concept.
///
/// `centroid` is the concept's centroid (`None` for a directory stand-in with
/// no embedding — then chunks are taken in file/line order, no similarity
/// ranking). `allowed_files` is the concept's member files, optionally already
/// restricted by a §3.1 lens; only chunks under those files are eligible.
/// Ranking is cosine-to-centroid, then MMR diversification over chunk vectors.
pub fn select_member_chunks(
    centroid: Option<&[f32]>,
    chunks: &[IndexedChunk],
    allowed_files: &HashSet<String>,
    k: usize,
) -> Vec<ConceptSnippet> {
    if k == 0 || allowed_files.is_empty() {
        return Vec::new();
    }
    // Eligible chunks: those whose path is a concept member file.
    let mut pool: Vec<(&IndexedChunk, f32)> = chunks
        .iter()
        .filter(|c| allowed_files.contains(&c.path))
        .map(|c| {
            let rel = match centroid {
                Some(ce) => cosine(ce, &c.vector),
                None => 0.0,
            };
            (c, rel)
        })
        .collect();
    if pool.is_empty() {
        return Vec::new();
    }
    // Sort by relevance desc (stable file/idx order when no centroid).
    pool.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.path.cmp(&b.0.path))
            .then_with(|| a.0.chunk_idx.cmp(&b.0.chunk_idx))
    });

    // MMR diversification over chunk vectors (skip when no centroid — order is
    // already the stable fallback).
    let selected: Vec<(&IndexedChunk, f32)> = if centroid.is_some() {
        mmr(pool, k)
    } else {
        pool.into_iter().take(k).collect()
    };

    selected
        .into_iter()
        .map(|(c, rel)| ConceptSnippet {
            path: c.path.clone(),
            start_line: c.start_line,
            end_line: c.end_line,
            content: c.content.clone(),
            score: rel,
        })
        .collect()
}

/// Greedy MMR: seed with the top hit, then repeatedly pick the candidate
/// maximising `λ·rel − (1−λ)·max cosine-to-selected`.
fn mmr(mut candidates: Vec<(&IndexedChunk, f32)>, k: usize) -> Vec<(&IndexedChunk, f32)> {
    let target = k.min(candidates.len());
    if target == 0 {
        return Vec::new();
    }
    let mut selected: Vec<(&IndexedChunk, f32)> = Vec::with_capacity(target);
    selected.push(candidates.remove(0));
    while selected.len() < target && !candidates.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f32::NEG_INFINITY;
        for (i, (cand, rel)) in candidates.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|(s, _)| cosine(&cand.vector, &s.vector))
                .fold(0.0f32, f32::max);
            let score = MMR_LAMBDA * rel - (1.0 - MMR_LAMBDA) * max_sim;
            if score > best_mmr {
                best_mmr = score;
                best_idx = i;
            }
        }
        selected.push(candidates.remove(best_idx));
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(path: &str, idx: u32, v: Vec<f32>) -> IndexedChunk {
        IndexedChunk {
            id: format!("{path}:{idx}"),
            path: path.to_owned(),
            chunk_idx: idx,
            content: format!("body {path}:{idx}"),
            mtime: 0.0,
            start_line: idx * 10 + 1,
            end_line: idx * 10 + 9,
            vector: v,
        }
    }

    fn files(fs: &[&str]) -> HashSet<String> {
        fs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ranks_by_cosine_to_centroid() {
        let centroid = vec![1.0, 0.0];
        let chunks = vec![
            chunk("a.rs", 0, vec![0.0, 1.0]), // orthogonal — low
            chunk("a.rs", 1, vec![1.0, 0.0]), // aligned — high
        ];
        let out = select_member_chunks(Some(&centroid), &chunks, &files(&["a.rs"]), 2);
        assert_eq!(
            out[0].start_line, 11,
            "the centroid-aligned chunk ranks first"
        );
        assert!(out[0].score > out[1].score);
    }

    #[test]
    fn restricts_to_allowed_member_files() {
        let centroid = vec![1.0, 0.0];
        let chunks = vec![
            chunk("in.rs", 0, vec![1.0, 0.0]),
            chunk("out.rs", 0, vec![1.0, 0.0]),
        ];
        let out = select_member_chunks(Some(&centroid), &chunks, &files(&["in.rs"]), 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "in.rs", "out-of-concept file excluded");
    }

    #[test]
    fn mmr_diversifies_near_duplicates() {
        // Centroid equidistant from all three (cosine 0.707 each), so MMR's
        // diversity term — not raw relevance — decides the second pick.
        let centroid = vec![1.0, 1.0, 0.0];
        let chunks = vec![
            chunk("a.rs", 0, vec![1.0, 0.0, 0.0]), // seed (top, tie broken by idx)
            chunk("a.rs", 1, vec![1.0, 0.0, 0.0]), // exact duplicate of the seed
            chunk("a.rs", 2, vec![0.0, 1.0, 0.0]), // orthogonal to the seed — fresh
        ];
        let out = select_member_chunks(Some(&centroid), &chunks, &files(&["a.rs"]), 2);
        let picked: Vec<u32> = out.iter().map(|s| (s.start_line - 1) / 10).collect();
        assert_eq!(picked[0], 0, "top hit seeds");
        assert_eq!(
            picked[1], 2,
            "MMR picks the diverse chunk, not the duplicate"
        );
    }

    #[test]
    fn no_centroid_falls_back_to_stable_order() {
        let chunks = vec![
            chunk("b.rs", 1, vec![0.0, 1.0]),
            chunk("a.rs", 0, vec![1.0, 0.0]),
        ];
        let out = select_member_chunks(None, &chunks, &files(&["a.rs", "b.rs"]), 5);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].path, "a.rs",
            "stable path/idx order without a centroid"
        );
    }

    #[test]
    fn empty_inputs_are_safe() {
        assert!(select_member_chunks(None, &[], &files(&["a"]), 5).is_empty());
        assert!(
            select_member_chunks(None, &[chunk("a", 0, vec![1.0])], &HashSet::new(), 5).is_empty()
        );
        assert!(
            select_member_chunks(None, &[chunk("a", 0, vec![1.0])], &files(&["a"]), 0).is_empty()
        );
    }
}
