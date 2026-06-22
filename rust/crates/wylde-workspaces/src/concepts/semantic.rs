//! Build *semantic* concepts by embedding-clustering the chunk vectors (TBS
//! concept-system Phase 2, thesis §7 S2.1/S2.2/S2.4).
//!
//! This is the concept-source upgrade: instead of labeling directory clusters
//! ([`super::cheap`]), cluster the workspace's `chunks.jsonl` embedding vectors
//! ([`super::clustering`]) into overlapping themes, compute each theme's
//! centroid, and emit [`Concept`]s carrying that centroid (so query→concept
//! routing — a later, deferred phase — has something to score against).
//!
//! Membership is **file-grained**: the chunk store is file-fragment-grained
//! (an `IndexedChunk` has no per-chunk symbol/entity list), so a semantic
//! concept's `members`/`member_files` are the distinct source files of its
//! cluster's chunks. Symbol-grained membership awaits a chunk→entity map; the
//! centroid (the routing prize) and the file set (the browse "files involved")
//! are exact today.
//!
//! ## Overlap (S2.4)
//!
//! [`super::clustering::soft_members`] gives many-to-many membership — a chunk
//! near two centroids contributes its file to both concepts, realising the
//! thesis's "concepts are tags, not a partition." `MEMBER` overlap is therefore
//! structural, not enforced-unique.
//!
//! ## Labeling (S2.2)
//!
//! Labels are derived **heuristically** from the dominant directory of a
//! cluster's files (deterministic, offline, testable). The thesis envisions an
//! LLM naming each cluster; that is a fail-soft *enrichment* layered on top
//! ([`label_for_files`] is the seam) — it never gates the build, so a down
//! Ollama still yields a usable, named concept set.

use std::collections::BTreeMap;

use super::clustering::{self, default_k};
use super::concept::{Concept, ConceptSource};
use crate::rag::indexer::store::IndexedChunk;

/// The id prefix marking a semantic (embedding-clustered) concept.
pub const SEM_CONCEPT_PREFIX: &str = "sem:";

/// Default greedy nearest-centroid **carry-over** threshold (Phase-B stable
/// ids): a new cluster whose centroid is ≥ this cosine to a prior `sem:`
/// concept's centroid **reuses that id** (same theme, drifted). Tunable via
/// [`SemanticParams::carry_over_threshold`]. 0.85 tolerates the small centroid
/// drift a "same" theme shows between recomputes while staying well clear of
/// distinct themes (which typically sit far below 0.85 cosine).
pub const DEFAULT_CARRY_OVER_THRESHOLD: f32 = 0.85;

/// Tunables for a semantic build. Defaults are deterministic + offline.
#[derive(Clone, Debug)]
pub struct SemanticParams {
    /// Cluster count; `None` → [`default_k`] (≈ √n).
    pub k: Option<usize>,
    /// k-means iterations.
    pub iters: usize,
    /// Soft-membership cosine margin (overlap). 0 = hard partition.
    pub overlap_margin: f32,
    /// RNG seed (fixed ⇒ reproducible clusters).
    pub seed: u64,
    /// Cosine threshold for carrying a prior `sem:` id over to a new cluster
    /// ([`DEFAULT_CARRY_OVER_THRESHOLD`]).
    pub carry_over_threshold: f32,
}

impl Default for SemanticParams {
    fn default() -> Self {
        Self {
            k: None,
            iters: 25,
            overlap_margin: 0.04,
            seed: 0x5EED,
            carry_over_threshold: DEFAULT_CARRY_OVER_THRESHOLD,
        }
    }
}

/// A stable-id build result: the concept set plus the advanced **never-reused**
/// `sem:` ordinal allocator (persist it so a deleted theme's number is never
/// recycled onto a different theme — Phase-B §4.1).
#[derive(Clone, Debug, PartialEq)]
pub struct StableBuild {
    pub concepts: Vec<Concept>,
    pub next_ordinal: u32,
}

/// Build semantic concepts from a workspace's chunks. Pure + deterministic.
/// Convenience wrapper over [`build_semantic_concepts_stable`] with **no prior
/// concepts** (fresh ids from ordinal 0) — used by tests and any caller that
/// doesn't need cross-recompute id stability.
pub fn build_semantic_concepts(chunks: &[IndexedChunk], params: &SemanticParams) -> Vec<Concept> {
    build_semantic_concepts_stable(chunks, params, &[], 0).concepts
}

/// Build semantic concepts with **stable ids** (Phase-B §4.1). Pure +
/// deterministic. Each new cluster greedily claims the nearest prior `sem:`
/// concept's id when their centroids are ≥ `params.carry_over_threshold` cosine
/// (one-to-one, descending similarity) — so a relation authored on a theme
/// survives a recompute that merely grows/drifts the corpus. Genuinely-new
/// clusters mint a fresh id from the monotonic `next_ordinal` allocator, never
/// reusing a number. Chunks without a usable vector are skipped; a degenerate
/// input yields no concepts.
pub fn build_semantic_concepts_stable(
    chunks: &[IndexedChunk],
    params: &SemanticParams,
    prior: &[Concept],
    next_ordinal: u32,
) -> StableBuild {
    // Keep only chunks with a non-empty vector, and pin the dimension to the
    // first one (defends against a torn/mixed index).
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut dim = 0usize;
    for c in chunks {
        if c.vector.is_empty() {
            continue;
        }
        if dim == 0 {
            dim = c.vector.len();
        }
        if c.vector.len() != dim {
            continue;
        }
        vectors.push(c.vector.clone());
        files.push(c.path.clone());
    }
    if vectors.len() < 2 {
        return StableBuild {
            concepts: Vec::new(),
            next_ordinal,
        };
    }

    let k = params.k.unwrap_or_else(|| default_k(vectors.len()));
    let res = clustering::cluster(&vectors, k, params.iters, params.seed);
    let soft = clustering::soft_members(&vectors, &res.centroids, params.overlap_margin);

    // Build draft concepts (no id yet) for every non-empty cluster.
    let mut drafts: Vec<Concept> = Vec::new();
    for (ci, members) in soft.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let mut cluster_files: Vec<String> = members.iter().map(|&i| files[i].clone()).collect();
        cluster_files.sort();
        cluster_files.dedup();

        let (label, description) = label_for_files(&cluster_files, members.len());
        // Placeholder id; the carry-over pass assigns the real one below.
        let mut concept = Concept::new("", label, description, ConceptSource::Embedding);
        concept.members = cluster_files.clone();
        concept.member_files = cluster_files;
        // Carry the centroid — the routing prize + the carry-over key.
        let mut centroid = res.centroids[ci].clone();
        let norm_sq: f32 = centroid.iter().map(|x| x * x).sum();
        if norm_sq > 0.0 {
            let inv = 1.0 / norm_sq.sqrt();
            for x in &mut centroid {
                *x *= inv;
            }
            concept.centroid = Some(centroid);
        }
        drafts.push(concept);
    }

    let next_ordinal = assign_stable_ids(&mut drafts, prior, params.carry_over_threshold, next_ordinal);

    drafts.sort_by(|a, b| a.id.cmp(&b.id));
    disambiguate_labels(&mut drafts);
    StableBuild {
        concepts: drafts,
        next_ordinal,
    }
}

/// Greedily carry prior `sem:` ids onto the nearest new draft by centroid
/// cosine (≥ `tau`, one-to-one), then mint fresh never-reused ids for the rest.
/// Returns the advanced ordinal allocator. Deterministic: ties break by draft
/// then prior index.
fn assign_stable_ids(
    drafts: &mut [Concept],
    prior: &[Concept],
    tau: f32,
    next_ordinal: u32,
) -> u32 {
    // Prior semantic concepts with a usable centroid are the carry-over pool.
    let pool: Vec<&Concept> = prior
        .iter()
        .filter(|c| {
            c.source == ConceptSource::Embedding
                && c.id.starts_with(SEM_CONCEPT_PREFIX)
                && c.centroid.is_some()
        })
        .collect();

    // All (cosine, draft_idx, prior_idx) candidate pairs, dimension-compatible.
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (di, d) in drafts.iter().enumerate() {
        let Some(dc) = d.centroid.as_ref() else {
            continue;
        };
        for (pi, p) in pool.iter().enumerate() {
            let pc = p.centroid.as_ref().expect("filtered to Some");
            if pc.len() == dc.len() {
                pairs.push((cosine(dc, pc), di, pi));
            }
        }
    }
    // Descending similarity; deterministic tie-break.
    pairs.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });

    let mut claimed_id: Vec<Option<String>> = vec![None; drafts.len()];
    let mut prior_used = vec![false; pool.len()];
    for (cos, di, pi) in pairs {
        if cos < tau {
            break; // sorted desc — nothing below tau can match
        }
        if claimed_id[di].is_none() && !prior_used[pi] {
            claimed_id[di] = Some(pool[pi].id.clone());
            prior_used[pi] = true;
        }
    }

    // Never reuse a number: start the allocator past every prior ordinal AND
    // the persisted high-water mark, so a dropped theme's id can't be recycled.
    let max_prior = pool
        .iter()
        .filter_map(|c| c.id.strip_prefix(SEM_CONCEPT_PREFIX))
        .filter_map(|s| s.parse::<u32>().ok())
        .max();
    let mut next = next_ordinal.max(max_prior.map(|m| m + 1).unwrap_or(0));

    for (di, d) in drafts.iter_mut().enumerate() {
        match claimed_id[di].take() {
            Some(id) => d.id = id,
            None => {
                d.id = format!("{SEM_CONCEPT_PREFIX}{next:04}");
                next += 1;
            }
        }
    }
    next
}

/// Cosine of two equal-length vectors (centroids are unit-normalised, so this
/// is their dot product). Returns 0.0 for a zero vector.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Disambiguate **colliding base labels** (viz-fix A.1). k-means partitions the
/// embedding space, which doesn't map 1:1 onto directories, so several
/// semantically-distinct clusters can each have their plurality of files under
/// the same folder and all humanise to the same name ("Composer ×3"). The base
/// labeller ([`label_for_files`]) has no cross-cluster awareness, so it can't
/// tell them apart.
///
/// This post-pass runs after the whole set is built: for any base label shared
/// by more than one concept, append a distinguishing token derived from each
/// cluster's *runner-up* directory leaf (or, failing that, a distinctive
/// filename stem) — `Composer · models`, `Composer · ipc`, `Composer · render`.
/// Any labels that are *still* identical afterwards (genuinely
/// indistinguishable by directory/file signal) get the cluster-id tail so they
/// remain individually addressable. Deterministic and offline.
fn disambiguate_labels(concepts: &mut [Concept]) {
    use std::collections::BTreeMap;

    // How many concepts share each base label?
    let mut base_counts: BTreeMap<String, usize> = BTreeMap::new();
    for c in concepts.iter() {
        *base_counts.entry(c.label.clone()).or_default() += 1;
    }

    // First pass: append the runner-up token to every colliding base label.
    for c in concepts.iter_mut() {
        if base_counts.get(&c.label).copied().unwrap_or(0) <= 1 {
            continue;
        }
        if let Some(tok) = distinguishing_token(&c.member_files) {
            c.label = format!("{} · {tok}", c.label);
        }
    }

    // Second pass: anything still duplicated (same runner-up token, or no
    // directory/file signal at all) gets the id tail so it stays addressable.
    let mut final_counts: BTreeMap<String, usize> = BTreeMap::new();
    for c in concepts.iter() {
        *final_counts.entry(c.label.clone()).or_default() += 1;
    }
    for c in concepts.iter_mut() {
        if final_counts.get(&c.label).copied().unwrap_or(0) <= 1 {
            continue;
        }
        let tail = c.id.rsplit(':').next().unwrap_or(c.id.as_str());
        c.label = format!("{} · {tail}", c.label);
    }
}

/// A token that distinguishes one cluster from its same-named siblings: the
/// **runner-up** parent-directory leaf (the most common dir that isn't the
/// dominant one), else the most common filename **stem** token. Humanised.
/// `None` only when the cluster has no usable path signal at all.
fn distinguishing_token(files: &[String]) -> Option<String> {
    let mut leaves: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        if let Some(leaf) = parent_leaf(f) {
            *leaves.entry(leaf).or_default() += 1;
        }
    }
    // Dominant uses the same selection rule as `label_for_files`.
    let dominant = leaves
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| k.clone());
    // Runner-up = highest-count leaf that isn't the dominant one.
    let runner = leaves
        .iter()
        .filter(|(k, _)| Some(k.as_str()) != dominant.as_deref())
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| k.clone());
    if let Some(r) = runner {
        return Some(humanize(&r));
    }
    // Single-directory cluster: fall back to the most common filename stem.
    file_stem_token(files)
}

/// The most common filename stem across `files` (filename without its
/// extension), humanised; ties break lexicographically. The single-directory
/// disambiguation fallback.
fn file_stem_token(files: &[String]) -> Option<String> {
    let mut stems: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        let norm = f.replace('\\', "/");
        let name = norm.rsplit('/').next().unwrap_or(&norm);
        let stem = name.split('.').next().unwrap_or(name);
        if !stem.is_empty() {
            *stems.entry(stem.to_owned()).or_default() += 1;
        }
    }
    stems
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| humanize(k))
}

/// Derive a `(label, description)` for a cluster from its files' dominant
/// directory (heuristic; the LLM-labeling seam, thesis S2.2). The label is the
/// humanised most-common parent-directory leaf; the description names the
/// spread.
pub fn label_for_files(files: &[String], chunk_count: usize) -> (String, String) {
    // Tally parent-directory leaves across the files.
    let mut leaves: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        if let Some(leaf) = parent_leaf(f) {
            *leaves.entry(leaf).or_default() += 1;
        }
    }
    let dominant = leaves
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| k.clone());

    let label = match &dominant {
        Some(d) => humanize(d),
        None => "Theme".to_owned(),
    };
    let spread = if leaves.len() <= 1 {
        String::new()
    } else {
        format!(" spanning {} directories", leaves.len())
    };
    let description = format!(
        "Semantic cluster of {} file(s){spread}{}. Embedding-derived concept.",
        files.len(),
        match &dominant {
            Some(d) => format!(", mostly `{d}`"),
            None => String::new(),
        }
    );
    let _ = chunk_count; // reserved for a richer description / LLM prompt later
    (label, description)
}

/// The last directory component of a file path (handles `/` and `\`).
fn parent_leaf(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let mut parts: Vec<&str> = norm.split('/').collect();
    parts.pop(); // drop the filename
    parts
        .into_iter()
        .rev()
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Title-case a path segment: split on `_`/`-`/space, capitalise each word.
fn humanize(seg: &str) -> String {
    let words: Vec<String> = seg
        .split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        seg.to_owned()
    } else {
        words.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, path: &str, vec: Vec<f32>) -> IndexedChunk {
        IndexedChunk {
            id: id.to_owned(),
            path: path.to_owned(),
            chunk_idx: 0,
            content: String::new(),
            mtime: 0.0,
            start_line: 1,
            end_line: 1,
            vector: vec,
        }
    }

    fn norm(mut v: Vec<f32>) -> Vec<f32> {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            for x in &mut v {
                *x /= n;
            }
        }
        v
    }

    /// Two clearly-separated semantic groups, each in its own directory.
    fn corpus() -> Vec<IndexedChunk> {
        let mut cs = Vec::new();
        for j in 0..6 {
            cs.push(chunk(
                &format!("a{j}"),
                &format!("src/auth/file{}.rs", j % 2),
                norm(vec![1.0, 0.05 * j as f32, 0.0]),
            ));
        }
        for j in 0..6 {
            cs.push(chunk(
                &format!("g{j}"),
                &format!("src/graph/file{}.rs", j % 2),
                norm(vec![0.0, 0.05 * j as f32, 1.0]),
            ));
        }
        cs
    }

    #[test]
    fn clusters_into_semantic_concepts_with_centroids() {
        let concepts = build_semantic_concepts(
            &corpus(),
            &SemanticParams {
                k: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(concepts.len(), 2, "two themes");
        for c in &concepts {
            assert_eq!(c.source, ConceptSource::Embedding);
            assert!(
                c.centroid.as_ref().is_some_and(|v| v.len() == 3),
                "centroid carried"
            );
            assert!(c.id.starts_with("sem:"));
            assert!(!c.member_files.is_empty());
        }
    }

    #[test]
    fn members_are_the_clusters_files() {
        let concepts = build_semantic_concepts(
            &corpus(),
            &SemanticParams {
                k: Some(2),
                overlap_margin: 0.0,
                ..Default::default()
            },
        );
        // With a hard partition, the auth files and graph files separate.
        let auth = concepts
            .iter()
            .find(|c| c.member_files.iter().all(|f| f.contains("/auth/")))
            .expect("an auth-only concept");
        assert!(auth.member_files.iter().all(|f| f.contains("/auth/")));
    }

    #[test]
    fn labels_reflect_dominant_directory() {
        let (label, desc) = label_for_files(
            &[
                "src/graph_writer/a.rs".into(),
                "src/graph_writer/b.rs".into(),
            ],
            4,
        );
        assert_eq!(label, "Graph Writer");
        assert!(desc.contains("mostly `graph_writer`"));
        assert!(desc.contains("2 file(s)"));
    }

    fn concept_with_files(id: &str, label: &str, files: &[&str]) -> Concept {
        let mut c = Concept::new(id, label, "", ConceptSource::Embedding);
        c.member_files = files.iter().map(|s| (*s).to_owned()).collect();
        c
    }

    #[test]
    fn collisions_disambiguated_by_runner_up_dir() {
        let mut concepts = vec![
            concept_with_files(
                "sem:0000",
                "Composer",
                &["src/composer/a.rs", "src/composer/b.rs", "src/models/x.rs"],
            ),
            concept_with_files(
                "sem:0001",
                "Composer",
                &["src/composer/c.rs", "src/composer/d.rs", "src/ipc/y.rs"],
            ),
            concept_with_files("sem:0002", "Graph", &["src/graph/g.rs"]),
        ];
        disambiguate_labels(&mut concepts);
        // The lone "Graph" is left untouched.
        assert_eq!(concepts[2].label, "Graph");
        // Each "Composer" gets its runner-up directory appended.
        assert_eq!(concepts[0].label, "Composer · Models");
        assert_eq!(concepts[1].label, "Composer · Ipc");
        // And nothing collides anymore.
        let labels: Vec<_> = concepts.iter().map(|c| &c.label).collect();
        let mut uniq = labels.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(labels.len(), uniq.len(), "all labels distinct");
    }

    #[test]
    fn unresolvable_collisions_fall_back_to_id_tail() {
        // Two clusters entirely inside the same single directory with the same
        // filename stem — no runner-up dir, identical stem — must still end up
        // distinct via the cluster-id tail.
        let mut concepts = vec![
            concept_with_files("sem:0007", "Conf", &["src/conf/mod.rs"]),
            concept_with_files("sem:0008", "Conf", &["src/conf/mod.rs"]),
        ];
        disambiguate_labels(&mut concepts);
        assert_ne!(concepts[0].label, concepts[1].label);
        assert!(concepts[0].label.ends_with("0007"), "{}", concepts[0].label);
        assert!(concepts[1].label.ends_with("0008"), "{}", concepts[1].label);
    }

    #[test]
    fn deterministic_across_runs() {
        let a = build_semantic_concepts(&corpus(), &SemanticParams::default());
        let b = build_semantic_concepts(&corpus(), &SemanticParams::default());
        // Compare structure + centroids; `Concept::new` timestamps legitimately
        // differ across calls (see the cheap-builder determinism test).
        let strip = |cs: &[Concept]| -> Vec<(String, Vec<String>, Option<Vec<f32>>)> {
            cs.iter()
                .map(|c| (c.id.clone(), c.member_files.clone(), c.centroid.clone()))
                .collect()
        };
        assert_eq!(strip(&a), strip(&b));
    }

    #[test]
    fn too_few_chunks_yields_nothing() {
        assert!(build_semantic_concepts(&[], &SemanticParams::default()).is_empty());
        let one = vec![chunk("x", "a.rs", norm(vec![1.0, 0.0]))];
        assert!(build_semantic_concepts(&one, &SemanticParams::default()).is_empty());
    }

    #[test]
    fn stable_ids_carry_over_on_same_corpus_recompute() {
        let params = SemanticParams {
            k: Some(2),
            overlap_margin: 0.0,
            ..Default::default()
        };
        let first = build_semantic_concepts_stable(&corpus(), &params, &[], 0);
        assert_eq!(first.concepts.len(), 2);
        // Recompute over the same corpus, feeding the prior concepts back in:
        // identical centroids ⇒ every id carries over (cosine 1.0 ≥ τ).
        let second =
            build_semantic_concepts_stable(&corpus(), &params, &first.concepts, first.next_ordinal);
        let ids_a: Vec<&str> = first.concepts.iter().map(|c| c.id.as_str()).collect();
        let ids_b: Vec<&str> = second.concepts.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids_a, ids_b, "same corpus ⇒ ids carried over verbatim");
        // No new ids minted ⇒ the allocator doesn't advance.
        assert_eq!(second.next_ordinal, first.next_ordinal);
    }

    #[test]
    fn carry_over_reuses_id_for_drifted_theme() {
        let params = SemanticParams {
            k: Some(2),
            overlap_margin: 0.0,
            ..Default::default()
        };
        let first = build_semantic_concepts_stable(&corpus(), &params, &[], 0);
        // Pick one prior concept and slightly drift its centroid (still ≥ 0.85
        // cosine to its own theme). Its id must be carried over, not re-minted.
        let target = &first.concepts[0];
        let drifted_centroid: Vec<f32> = norm(
            target
                .centroid
                .as_ref()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, x)| x + if i == 1 { 0.05 } else { 0.0 })
                .collect(),
        );
        let mut prior = first.concepts.clone();
        prior[0].centroid = Some(drifted_centroid);
        let second =
            build_semantic_concepts_stable(&corpus(), &params, &prior, first.next_ordinal);
        let ids: std::collections::HashSet<&str> =
            second.concepts.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(target.id.as_str()), "drifted theme keeps its id");
    }

    #[test]
    fn new_theme_mints_fresh_never_reused_id() {
        // A prior concept whose centroid matches NEITHER corpus theme (vanished
        // theme). Its number must not be recycled; new themes mint past the
        // allocator high-water mark.
        let mut gone = Concept::new("sem:0005", "Gone", "d", ConceptSource::Embedding);
        gone.centroid = Some(norm(vec![0.0, 1.0, 0.0])); // orthogonal-ish to both
        let params = SemanticParams {
            k: Some(2),
            overlap_margin: 0.0,
            ..Default::default()
        };
        let out = build_semantic_concepts_stable(&corpus(), &params, &[gone], 6);
        let ids: Vec<&str> = out.concepts.iter().map(|c| c.id.as_str()).collect();
        assert!(
            ids.iter().all(|id| *id != "sem:0005"),
            "vanished theme's id is never reused: {ids:?}"
        );
        assert!(
            ids.contains(&"sem:0006") && ids.contains(&"sem:0007"),
            "new themes mint past the allocator: {ids:?}"
        );
        assert_eq!(out.next_ordinal, 8, "allocator advanced by the two new ids");
    }

    #[test]
    fn skips_chunks_without_vectors() {
        let mut cs = corpus();
        cs.push(chunk("empty", "src/x/none.rs", vec![]));
        let concepts = build_semantic_concepts(
            &cs,
            &SemanticParams {
                k: Some(2),
                ..Default::default()
            },
        );
        // The vector-less chunk's file never appears as a member.
        assert!(concepts
            .iter()
            .all(|c| !c.member_files.iter().any(|f| f.contains("none.rs"))));
    }
}
