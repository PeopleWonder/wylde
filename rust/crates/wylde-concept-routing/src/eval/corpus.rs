//! The in-memory eval corpus — the file/chunk index, the concepts, the
//! relation graph, and the vocabulary the arms run over.
//!
//! Pure: the corpus is *built* impurely (the live runner reads the persisted
//! `chunks.jsonl` + decrypts `concepts.json`; a test builds a synthetic one),
//! but everything here operates on owned in-memory data — no I/O, no embed, no
//! service. That is what lets the same arm/metric code run identically over the
//! live index and over a controlled fixture.
//!
//! ## Path matching
//!
//! Gold files are repo-relative, lowercase, forward-slash (e.g.
//! `rust/crates/wylde-harness/src/turn/tool_round.rs`). Live chunk/member paths
//! are absolute Windows paths with a `\\?\` long-path prefix. [`normalize_path`]
//! collapses both to a lowercase forward-slash form; a retrieved path **matches**
//! a gold path when it ends with the gold suffix ([`path_matches`]).

use std::collections::HashMap;

use crate::relations::{NodeRef, RelationGraph};
use crate::router::{score::cosine, ConceptCentroid};

/// One indexed chunk: where it lives + its embedding + an estimated token cost
/// (for the token-cost metric). Content is **not** retained — only what the
/// metrics need — so a 14k-chunk live corpus stays ~tens of MB.
#[derive(Clone, Debug)]
pub struct EvalChunk {
    /// Normalised path (lowercase, forward-slash, no long-path prefix).
    pub path: String,
    pub vector: Vec<f32>,
    /// Estimated tokens for this chunk (~chars/4), summed into an arm's cost.
    pub tokens: usize,
}

/// A concept reduced to what the eval needs: identity, centroid, member files
/// (normalised), and `described_by` vocab links (for the seed-lift).
#[derive(Clone, Debug)]
pub struct EvalConcept {
    pub id: String,
    pub label: String,
    pub centroid: Vec<f32>,
    pub member_files: Vec<String>,
    pub described_by: Vec<String>,
}

/// The whole corpus an arm runs over.
#[derive(Clone, Debug, Default)]
pub struct EvalCorpus {
    pub chunks: Vec<EvalChunk>,
    pub concepts: Vec<EvalConcept>,
    /// Authored typed relations (conflation excludes + dependency edges). Empty
    /// for the seed-only ablation arm.
    pub relations: RelationGraph,
    /// Vocabulary identifiers `match_vocabulary` scans the query against.
    pub vocab_terms: Vec<String>,
}

impl EvalCorpus {
    /// The concept centroids in the router's input shape.
    pub fn concept_centroids(&self) -> Vec<ConceptCentroid> {
        self.concepts
            .iter()
            .map(|c| ConceptCentroid {
                id: c.id.clone(),
                label: c.label.clone(),
                centroid: c.centroid.clone(),
                described_by: c.described_by.clone(),
            })
            .collect()
    }

    /// For each file, the **best cosine** of any of its chunks to `query_vec`
    /// (computed once over every chunk, reused to rank both the baseline files
    /// and the files inside a routed concept). Files with no chunk are absent.
    pub fn file_best_cosine(&self, query_vec: &[f32]) -> HashMap<String, f32> {
        let mut best: HashMap<String, f32> = HashMap::new();
        for ch in &self.chunks {
            let c = cosine(query_vec, &ch.vector);
            best.entry(ch.path.clone())
                .and_modify(|b| {
                    if c > *b {
                        *b = c;
                    }
                })
                .or_insert(c);
        }
        best
    }

    /// Baseline ranking: every chunk by cosine to `query_vec`, descending,
    /// de-duplicated to files (a file ranks at its best chunk). Returns
    /// `(file_path, best_cosine)` best-first. Deterministic (path tiebreak).
    pub fn baseline_ranked_files(&self, query_vec: &[f32]) -> Vec<(String, f32)> {
        let mut files: Vec<(String, f32)> = self.file_best_cosine(query_vec).into_iter().collect();
        files.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        files
    }

    /// Map a file path to the concept ids whose member set contains it — used
    /// by the conflation/dependency metrics ("did the avoid concept get
    /// suppressed?"). Built once per corpus.
    pub fn file_to_concepts(&self) -> HashMap<String, Vec<String>> {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for c in &self.concepts {
            for f in &c.member_files {
                m.entry(f.clone()).or_default().push(c.id.clone());
            }
        }
        m
    }

    /// The concept ids that any of `gold_files` belongs to (suffix-matched
    /// against member files). The set a conflation/dependency case expects to
    /// be suppressed / pulled in.
    pub fn concepts_covering(&self, gold_files: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.concepts {
            if c.member_files
                .iter()
                .any(|m| gold_files.iter().any(|g| path_matches(m, g)))
                && !out.contains(&c.id)
            {
                out.push(c.id.clone());
            }
        }
        out
    }
}

/// Lowercase, forward-slashes, strip the Windows `\\?\` long-path prefix, trim a
/// trailing slash. Idempotent — gold paths (already in this form) pass through.
pub fn normalize_path(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    let mut s: String = p.replace('\\', "/").to_lowercase();
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// True when `path` (normalised) ends with the `gold` suffix (also normalised).
/// Suffix match so an absolute live path matches a repo-relative gold path,
/// guarded on a `/` boundary so `…/bolt.rs` doesn't match `…/turbolt.rs`.
pub fn path_matches(path: &str, gold: &str) -> bool {
    let p = normalize_path(path);
    let g = normalize_path(gold);
    if p == g {
        return true;
    }
    if p.ends_with(&g) {
        // Ensure the char before the suffix is a boundary.
        let idx = p.len() - g.len();
        return idx == 0 || p.as_bytes()[idx - 1] == b'/';
    }
    false
}

/// Grade a ranked list of file paths against a gold file list: entry `i` is
/// `true` iff `ranked[i]` matches any gold file. The relevance vector the
/// metrics consume.
pub fn grade(ranked: &[String], gold: &[String]) -> Vec<bool> {
    ranked
        .iter()
        .map(|r| gold.iter().any(|g| path_matches(r, g)))
        .collect()
}

/// How many distinct gold files a ranked list actually covers (the recall
/// denominator is the gold count, but a ranked list may match the same gold via
/// several paths — this counts distinct gold files hit, for diagnostics).
pub fn distinct_gold_hit(ranked: &[String], gold: &[String]) -> usize {
    gold.iter()
        .filter(|g| ranked.iter().any(|r| path_matches(r, g)))
        .count()
}

/// A vocab node ref for a matched term (convenience for corpus builders).
pub fn vocab_node(id: &str) -> NodeRef {
    NodeRef::vocab(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_prefix_and_lowercases() {
        assert_eq!(
            normalize_path(r"\\?\C:\Users\X\Wylde-release\rust\Crates\Foo.RS"),
            "c:/users/x/wylde-release/rust/crates/foo.rs"
        );
        // Idempotent on an already-normalised gold path.
        let g = "rust/crates/wylde-harness/src/turn/tool_round.rs";
        assert_eq!(normalize_path(g), g);
    }

    #[test]
    fn suffix_match_respects_boundary() {
        let abs = r"\\?\C:\x\rust\crates\wylde-workspaces\src\graph\bolt.rs";
        assert!(path_matches(abs, "src/graph/bolt.rs"));
        assert!(path_matches(abs, "bolt.rs"));
        // Boundary guard: must not match a longer filename ending in the suffix.
        assert!(!path_matches(r"\\?\C:\x\turbolt.rs", "bolt.rs"));
    }

    #[test]
    fn grade_marks_relevant_ranks() {
        let ranked = vec![
            r"C:\x\src\a.rs".to_string(),
            r"C:\x\src\b.rs".to_string(),
            r"C:\x\src\target.rs".to_string(),
        ];
        let gold = vec!["src/target.rs".to_string()];
        assert_eq!(grade(&ranked, &gold), vec![false, false, true]);
        assert_eq!(distinct_gold_hit(&ranked, &gold), 1);
    }

    #[test]
    fn file_best_cosine_takes_the_max_chunk() {
        let corpus = EvalCorpus {
            chunks: vec![
                EvalChunk {
                    path: "f.rs".into(),
                    vector: vec![1.0, 0.0],
                    tokens: 10,
                },
                EvalChunk {
                    path: "f.rs".into(),
                    vector: vec![0.0, 1.0],
                    tokens: 10,
                },
            ],
            ..EvalCorpus::default()
        };
        let q = vec![1.0, 0.0];
        let best = corpus.file_best_cosine(&q);
        assert!((best["f.rs"] - 1.0).abs() < 1e-6, "best chunk wins");
    }

    #[test]
    fn concepts_covering_finds_membership() {
        let corpus = EvalCorpus {
            concepts: vec![
                EvalConcept {
                    id: "c1".into(),
                    label: "C1".into(),
                    centroid: vec![1.0],
                    member_files: vec!["src/a.rs".into(), "src/b.rs".into()],
                    described_by: vec![],
                },
                EvalConcept {
                    id: "c2".into(),
                    label: "C2".into(),
                    centroid: vec![1.0],
                    member_files: vec!["src/z.rs".into()],
                    described_by: vec![],
                },
            ],
            ..EvalCorpus::default()
        };
        assert_eq!(corpus.concepts_covering(&["a.rs".into()]), vec!["c1"]);
        assert!(corpus.concepts_covering(&["nope.rs".into()]).is_empty());
    }
}
