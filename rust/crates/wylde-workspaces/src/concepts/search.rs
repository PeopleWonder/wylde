//! Hybrid concept search (thesis §3.2) — one primitive, two callers (the
//! Concepts browse tab **and**, later, chat retrieval).
//!
//!   * **Fuzzy half** — `nucleo_matcher` over each concept's label / id /
//!     description (the same engine `symbol_index` uses), normalised to
//!     `0.0..=1.0` against the top hit.
//!   * **Semantic half** — embed the query once (`nomic-embed-text`,
//!     [`crate::embeddings::embed_one`]) and cosine it against each concept's
//!     **centroid**. Directory stand-ins (Phase 0) carry no centroid, so the
//!     semantic half contributes nothing until Phase-2 clustering fills them —
//!     and we *skip embedding entirely* when no concept has a centroid, so the
//!     browse tab never blocks on Ollama in Phase 1.
//!   * **Fusion** — `combined = fuzzy + SEMANTIC_WEIGHT · semantic`, ranked
//!     descending. Fuzzy wins "I know the name"; semantic wins "I know the
//!     idea"; a concept strong on both ranks above either alone.
//!
//! [`rank_pure`] is the pure, unit-tested core (takes an already-embedded query
//! vector, or `None`); [`search`] is the async wrapper that loads the store and
//! does the (optional) embed.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::concept::Concept;
use super::store;

/// Weight on the semantic (centroid-cosine) half relative to the fuzzy half.
/// Fuzzy is the primary signal for a typed browse query; semantic is additive.
pub const SEMANTIC_WEIGHT: f32 = 0.6;

/// A concept with its fused score and the two component scores (for an
/// explainable UI — "matched on name" vs "matched on meaning").
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ScoredConcept {
    pub concept: Concept,
    pub score: f32,
    pub fuzzy: f32,
    pub semantic: f32,
}

/// Cosine of two equal-length vectors. Returns 0 for a length mismatch or a
/// zero vector. (Embeddings are L2-normalised, so this is just the dot product
/// in the common case — but we normalise defensively for centroids.)
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Best fuzzy score of `query` against a concept's label / id / description,
/// as a raw nucleo `u32` (0 = no match on any field).
fn raw_fuzzy(pattern: &Pattern, matcher: &mut Matcher, concept: &Concept) -> u32 {
    let mut buf = Vec::new();
    let mut best = 0u32;
    for field in [
        concept.label.as_str(),
        concept.id.as_str(),
        concept.description.as_str(),
    ] {
        if field.is_empty() {
            continue;
        }
        let hay = Utf32Str::new(field, &mut buf);
        if let Some(s) = pattern.score(hay, matcher) {
            best = best.max(s);
        }
    }
    best
}

/// Rank `concepts` for `query`. `query_vec` is the embedded query (for the
/// semantic half) or `None` to score fuzzy-only. Pure + deterministic.
///
/// * Empty `query` → every concept, ordered by label (a plain browse list).
/// * Non-empty `query` → only concepts that match on *some* signal (fuzzy hit
///   or non-trivial semantic similarity), ranked by the fused score, capped.
pub fn rank_pure(
    concepts: &[Concept],
    query: &str,
    query_vec: Option<&[f32]>,
    limit: usize,
) -> Vec<ScoredConcept> {
    let q = query.trim();
    if q.is_empty() {
        let mut all: Vec<ScoredConcept> = concepts
            .iter()
            .map(|c| ScoredConcept {
                concept: c.clone(),
                score: 0.0,
                fuzzy: 0.0,
                semantic: 0.0,
            })
            .collect();
        all.sort_by(|a, b| {
            a.concept
                .label
                .cmp(&b.concept.label)
                .then(a.concept.id.cmp(&b.concept.id))
        });
        all.truncate(limit);
        return all;
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);

    // Pass 1: raw fuzzy scores, to find the top for normalisation.
    let raw: Vec<u32> = concepts
        .iter()
        .map(|c| raw_fuzzy(&pattern, &mut matcher, c))
        .collect();
    let top = raw.iter().copied().max().unwrap_or(0).max(1) as f32;

    // The smallest semantic similarity that counts as a match on its own (so a
    // query with no fuzzy hit can still surface a meaning-near concept).
    const SEMANTIC_FLOOR: f32 = 0.3;

    let mut out: Vec<ScoredConcept> = Vec::new();
    for (c, raw_score) in concepts.iter().zip(raw.iter()) {
        let fuzzy = (*raw_score as f32 / top).clamp(0.0, 1.0);
        let semantic = match (query_vec, c.centroid.as_deref()) {
            (Some(qv), Some(cv)) => cosine(qv, cv).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let matched = *raw_score > 0 || semantic >= SEMANTIC_FLOOR;
        if !matched {
            continue;
        }
        out.push(ScoredConcept {
            concept: c.clone(),
            score: fuzzy + SEMANTIC_WEIGHT * semantic,
            fuzzy,
            semantic,
        });
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.concept.id.cmp(&b.concept.id))
    });
    out.truncate(limit);
    out
}

/// Whether any concept carries a centroid (⇒ the semantic half is worth an
/// embed round-trip). Phase-0 directory concepts have none.
fn any_centroid(concepts: &[Concept]) -> bool {
    concepts.iter().any(|c| c.centroid.is_some())
}

/// Search a workspace's concepts. Loads the store, embeds the query only when a
/// centroid exists (so Phase-1 browse never blocks on Ollama), and ranks.
/// Best-effort semantic: an embed failure degrades to fuzzy-only.
pub async fn search(workspace_id: &str, query: &str, limit: usize) -> Vec<ScoredConcept> {
    let concepts = store::load(workspace_id);
    if concepts.is_empty() {
        return Vec::new();
    }
    let query_vec: Option<Vec<f32>> = if !query.trim().is_empty() && any_centroid(&concepts) {
        crate::embeddings::embed_one(query.to_owned()).await.ok()
    } else {
        None
    };
    rank_pure(&concepts, query, query_vec.as_deref(), limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::ConceptSource;

    fn c(id: &str, label: &str, desc: &str) -> Concept {
        Concept::new(id, label, desc, ConceptSource::DirectoryCluster)
    }

    fn corpus() -> Vec<Concept> {
        vec![
            c("dir:src/auth", "Authentication", "login, tokens, sessions"),
            c("dir:src/graph", "Graph", "the code graph layer"),
            c("dir:src/rag", "Retrieval", "rag search over chunks"),
        ]
    }

    #[test]
    fn empty_query_returns_all_sorted_by_label() {
        let r = rank_pure(&corpus(), "", None, 10);
        let labels: Vec<&str> = r.iter().map(|s| s.concept.label.as_str()).collect();
        assert_eq!(labels, vec!["Authentication", "Graph", "Retrieval"]);
    }

    #[test]
    fn fuzzy_matches_label() {
        let r = rank_pure(&corpus(), "auth", None, 10);
        assert!(!r.is_empty());
        assert_eq!(r[0].concept.id, "dir:src/auth");
        assert!(r[0].fuzzy > 0.0);
        assert_eq!(r[0].semantic, 0.0, "no centroids ⇒ no semantic");
    }

    #[test]
    fn fuzzy_matches_description_words() {
        // "tokens" is only in the auth description.
        let r = rank_pure(&corpus(), "tokens", None, 10);
        assert!(r.iter().any(|s| s.concept.id == "dir:src/auth"));
    }

    #[test]
    fn non_matching_query_is_empty_without_semantic() {
        let r = rank_pure(&corpus(), "zzzzqqq", None, 10);
        assert!(r.is_empty());
    }

    #[test]
    fn semantic_lifts_a_concept_with_no_fuzzy_hit() {
        let mut cs = corpus();
        // Give "Graph" a centroid that exactly matches the query vector; the
        // query "zzzz" has no fuzzy hit anywhere, so only semantics can surface it.
        cs[1].centroid = Some(vec![1.0, 0.0, 0.0]);
        let qv = vec![1.0, 0.0, 0.0];
        let r = rank_pure(&cs, "zzzz", Some(&qv), 10);
        assert_eq!(r.len(), 1, "only the semantically-near concept surfaces");
        assert_eq!(r[0].concept.id, "dir:src/graph");
        assert!(r[0].semantic >= 0.99);
        assert_eq!(r[0].fuzzy, 0.0);
    }

    #[test]
    fn fusion_ranks_both_signals_above_one() {
        let mut cs = corpus();
        cs[0].centroid = Some(vec![1.0, 0.0]);
        let qv = vec![1.0, 0.0];
        let r = rank_pure(&cs, "authentication", Some(&qv), 10);
        let auth = r.iter().find(|s| s.concept.id == "dir:src/auth").unwrap();
        assert!(auth.fuzzy > 0.0 && auth.semantic > 0.0);
        assert!(auth.score > auth.fuzzy, "semantic adds on top of fuzzy");
    }

    #[test]
    fn limit_caps_results() {
        let r = rank_pure(&corpus(), "", None, 2);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn cosine_handles_mismatch_and_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }
}
