//! The routing decision (concept-routing plan §6.1) — *which* concepts a
//! query activates, plus the matched vocabulary, as one explainable
//! [`CandidateSet`].
//!
//! Pure + deterministic. [`route`] takes the **already-embedded** query vector
//! (the caller reuses the embed the RAG path already paid for — no extra
//! round-trip) and the workspace's concept centroids, cosines them
//! ([`score::cosine`]), and selects activations with the `dynamic_k`-shaped
//! cutoff ([`policy::select`]). Vocabulary is matched separately
//! ([`match_vocabulary`]) and concatenated in.
//!
//! **R1 produces the [`CandidateSet`] for logging only** — the caller
//! serialises it as threshold-calibration data and injects nothing. Injection
//! is R2.

pub mod policy;
pub mod score;
pub mod spread;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::relations::{NodeRef, RelationGraph};
use spread::Provenance;

/// A concept reduced to the two things routing needs: its identity (for the
/// explainable output) and its centroid (for scoring). The impure load — from
/// the encrypted concept store — happens in the workspaces routing bridge;
/// this crate never touches a file. A directory stand-in with no centroid is
/// simply never built into a `ConceptCentroid` (the bridge filters them out,
/// matching `concepts/search.rs` skipping centroid-less concepts).
#[derive(Debug, Clone, PartialEq)]
pub struct ConceptCentroid {
    /// Stable concept id within its workspace store (e.g. `dir:src/graph`).
    pub id: String,
    /// Human-readable label, carried through for the menu / log.
    pub label: String,
    /// The centroid embedding (non-empty; centroid-less concepts are excluded
    /// upstream).
    pub centroid: Vec<f32>,
    /// `DESCRIBED_BY` vocab identifiers ([`Concept::described_by`]). Used by the
    /// spreading-activation seed-lift (R1.5b): a matched vocab term lifts the
    /// concepts it names. Empty until a term is linked; the bridge populates it.
    pub described_by: Vec<String>,
}

/// One scored concept in the routed result. Both activated and suppressed
/// concepts appear (suppressed kept for the explainable curation menu + the
/// calibration log), distinguished by [`RoutedConcept::activated`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutedConcept {
    pub id: String,
    pub label: String,
    /// The **settled** activation after spreading (R1.5b) — what the cutoff is
    /// applied to. Equals [`Self::seed_score`] when no relation reshaped it
    /// (empty graph ⇒ identity).
    pub score: f32,
    /// The raw **seed** cosine of the query against this concept's centroid
    /// (the R1 "before" number) — kept beside [`Self::score`] so the calibration
    /// log can show before→after. Defaults to `score` for an older payload.
    #[serde(default)]
    pub seed_score: f32,
    /// Why this concept ended up where it did (seed / lifted / pulled via a
    /// dependency / co-activated / inhibited). Defaults to `Seed`.
    #[serde(default)]
    pub provenance: Provenance,
    /// Whether it cleared the cutoff (and the `max_concepts` cap).
    pub activated: bool,
}

/// A vocabulary/dictionary term the query mentioned. R1 matches on token
/// presence (score `1.0`); a later phase can weight by salience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VocabMatch {
    /// The matched term's identifier (an anchor `{{identifier}}` slug).
    pub identifier: String,
    /// Match strength in `[0, 1]`. R1: `1.0` for a whole-word token match.
    pub score: f32,
}

/// The explainable routing result — routed concepts (activated + suppressed)
/// concatenated with matched vocabulary, plus the cutoff decision. This is the
/// menu payload (R2) and, in R1, the structure logged as calibration data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateSet {
    /// The (possibly conversation-composed) query that was routed. Echoed for
    /// the log + the menu so a wrong activation is traceable to its query.
    pub query_echo: String,
    /// All scored concepts, **descending by score** (activated prefix first).
    pub concepts: Vec<RoutedConcept>,
    /// Matched vocabulary terms.
    pub vocabulary: Vec<VocabMatch>,
    /// The absolute floor in effect (config `abs_threshold`) — the chief
    /// number R4 calibrates.
    pub abs_threshold: f32,
    /// The actual cutoff a concept had to clear this turn:
    /// `max(abs_threshold, relative_floor · top)`, or `abs_threshold` when
    /// nothing cleared the absolute floor.
    pub chosen_cutoff: f32,
    /// Count of activated concepts (`0` ⇒ route nothing ⇒ raw-RAG fallback).
    pub activated_count: usize,
    /// The `max_concepts` cap in effect, for the log.
    pub max_concepts: usize,
}

impl CandidateSet {
    /// True when nothing activated — the caller falls back to raw-vector RAG
    /// (no behaviour change). Vocabulary matches alone do **not** count as an
    /// activation (they enrich the menu; they don't drive retrieval in R2's
    /// Augment mode).
    pub fn routed_nothing(&self) -> bool {
        self.activated_count == 0
    }

    /// Just the activated concepts, best-first — the set R2 would inject.
    pub fn activated(&self) -> impl Iterator<Item = &RoutedConcept> {
        self.concepts.iter().filter(|c| c.activated)
    }

    /// A compact, single-line summary for the R1 calibration log: the cutoff
    /// decision plus each concept's `label=score` (★ marks activated). Built
    /// for `tracing` so the threshold numbers land in the service log without a
    /// bespoke formatter at each call site.
    pub fn log_line(&self) -> String {
        let mut s = format!(
            "concept-routing: query={:?} cutoff={:.3} (abs={:.3}) activated={}/{} cap={} | ",
            truncate(&self.query_echo, 80),
            self.chosen_cutoff,
            self.abs_threshold,
            self.activated_count,
            self.concepts.len(),
            self.max_concepts,
        );
        let mut first = true;
        for c in &self.concepts {
            if !first {
                s.push_str(", ");
            }
            first = false;
            let mark = if c.activated { "★" } else { "·" };
            s.push_str(&format!("{mark}{}={:.3}", c.label, c.score));
        }
        if !self.vocabulary.is_empty() {
            s.push_str(" | vocab: ");
            s.push_str(
                &self
                    .vocabulary
                    .iter()
                    .map(|v| v.identifier.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        s
    }

    /// True when the relation graph actually reshaped the activation — any
    /// concept whose settled score differs from its seed cosine, or whose
    /// provenance isn't a plain `Seed`. When `false` the routing decision is
    /// identical to R1 (empty graph ⇒ identity).
    pub fn reshaped_by_relations(&self) -> bool {
        self.concepts.iter().any(|c| {
            (c.score - c.seed_score).abs() > 1e-6 || !matches!(c.provenance, Provenance::Seed)
        })
    }

    /// The R1.5b **before→after** proof line: for each concept, its seed cosine
    /// and its settled activation side by side, with a provenance glyph so the
    /// reshaping is visible — `★` activated, `·` suppressed, `↳` pulled in via a
    /// dependency, `↑` lifted by a vocab term, `+` positive co-activation, `⊘`
    /// inhibited by an exclusion. Logged beside [`log_line`](Self::log_line)
    /// when relations are in play so the calibration log shows the propagation,
    /// not just the seed.
    pub fn relation_log_line(&self) -> String {
        let mut s = format!(
            "concept-routing[relations]: query={:?} cutoff={:.3} activated={}/{} | ",
            truncate(&self.query_echo, 80),
            self.chosen_cutoff,
            self.activated_count,
            self.concepts.len(),
        );
        let mut first = true;
        for c in &self.concepts {
            if !first {
                s.push_str(", ");
            }
            first = false;
            let act = if c.activated { "★" } else { "·" };
            let (glyph, from) = match &c.provenance {
                Provenance::Seed => ("", String::new()),
                Provenance::SeedLift { from } => ("↑", from.label()),
                Provenance::Dependency { from, hops } => ("↳", format!("{}@{hops}", from.label())),
                Provenance::Positive { from } => ("+", from.label()),
                Provenance::Containment { from, hops } => ("⊂", format!("{}@{hops}", from.label())),
                Provenance::Inhibited { by, .. } => ("⊘", by.label()),
            };
            // before→after makes the lift/suppression unmistakable.
            s.push_str(&format!(
                "{act}{glyph}{}={:.3}→{:.3}",
                c.label, c.seed_score, c.score
            ));
            if !from.is_empty() {
                s.push_str(&format!("({from})"));
            }
        }
        s
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{}…", head.replace('\n', " "))
    }
}

/// Route `query_vec` against `concepts` under `cfg`, folding in pre-matched
/// `vocabulary` and the optional hierarchy `containment` adjacency (H6 — `&[]`
/// disables it; see [`spread`]). Pure: no I/O, no embed (the caller passes the
/// vector the RAG path already embedded — plan's "no extra round-trip").
///
/// Scores every concept by centroid cosine, sorts descending, applies the
/// `dynamic_k`-shaped [`policy::select`] cutoff, and marks the activated
/// prefix. All concepts are returned (activated + suppressed) so the result is
/// fully explainable for the calibration log and the later curation menu.
pub fn route(
    query_echo: impl Into<String>,
    query_vec: &[f32],
    concepts: &[ConceptCentroid],
    vocabulary: Vec<VocabMatch>,
    containment: &[(NodeRef, NodeRef)],
    graph: &RelationGraph,
    cfg: &crate::config::RoutingConfig,
) -> CandidateSet {
    let p = &cfg.relation_params;

    // ── Build the seed activation over the unified node space ──────────────
    // Concepts: their (seed_weight-scaled) centroid cosine. Matched vocab: the
    // match strength. `cosine_by_id` keeps the raw cosine for the before→after
    // calibration log (the R1 "seed-only" number).
    let mut seed_map: HashMap<NodeRef, f32> =
        HashMap::with_capacity(concepts.len() + vocabulary.len());
    let mut cosine_by_id: HashMap<String, f32> = HashMap::with_capacity(concepts.len());
    for c in concepts {
        let cos = score::cosine(query_vec, &c.centroid);
        cosine_by_id.insert(c.id.clone(), cos);
        seed_map.insert(NodeRef::concept(&c.id), p.seed_weight * cos);
    }
    for v in &vocabulary {
        seed_map.insert(NodeRef::vocab(&v.identifier), v.score);
    }

    // Seed-lift links: each matched-vocab term → the concepts that name it
    // (`described_by`). Only matched terms (those carrying a seed) lift.
    let matched: HashSet<&str> = vocabulary.iter().map(|v| v.identifier.as_str()).collect();
    let mut links: Vec<(NodeRef, NodeRef)> = Vec::new();
    for c in concepts {
        for term in &c.described_by {
            if matched.contains(term.as_str()) {
                links.push((NodeRef::vocab(term), NodeRef::concept(&c.id)));
            }
        }
    }

    // ── Spread (R1.5b): reshape the flat seed through the relation graph ────
    // Empty graph + no links + no containment ⇒ identity ⇒ settled activation ==
    // seed cosine, so `policy::select` sees exactly the R1 distribution
    // (behaviour-safe). `containment` (H6) is the hierarchy's separate channel,
    // empty unless the hierarchy toggle is ON and edges exist.
    let settled = spread::spread(seed_map, &links, containment, graph, p);

    // Each concept's SETTLED activation (what the cutoff is applied to) plus its
    // raw seed cosine and provenance. Sort descending by settled score with a
    // stable id tiebreak (deterministic log + menu — mirrors search.rs).
    let mut scored: Vec<RoutedConcept> = concepts
        .iter()
        .map(|c| {
            let node = NodeRef::concept(&c.id);
            RoutedConcept {
                id: c.id.clone(),
                label: c.label.clone(),
                score: settled.activation_of(&node),
                seed_score: cosine_by_id.get(&c.id).copied().unwrap_or(0.0),
                provenance: settled.provenance_of(&node),
                activated: false,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });

    let just_scores: Vec<f32> = scored.iter().map(|c| c.score).collect();
    let cutoff = policy::select(
        &just_scores,
        cfg.abs_threshold,
        cfg.relative_floor,
        cfg.max_concepts,
    );
    for c in scored.iter_mut().take(cutoff.activated) {
        c.activated = true;
    }

    CandidateSet {
        query_echo: query_echo.into(),
        concepts: scored,
        vocabulary,
        abs_threshold: cfg.abs_threshold,
        chosen_cutoff: cutoff.threshold,
        activated_count: cutoff.activated,
        max_concepts: cfg.max_concepts,
    }
}

/// Match dictionary/vocabulary `terms` (anchor identifiers) against `query` by
/// whole-word, case-insensitive token presence. The matched-vocabulary half of
/// the [`CandidateSet`] (plan §4). Pure; the bridge supplies the term list from
/// the workspace's anchor store.
///
/// "Whole word" so `{{rag}}` matches "how does rag work" but not "storage";
/// the term may itself be multi-word (matched as a contiguous phrase).
/// Deduplicated, capped, deterministic order (first appearance in `terms`).
pub fn match_vocabulary(query: &str, terms: &[String], limit: usize) -> Vec<VocabMatch> {
    let hay = format!(" {} ", normalize(query));
    let mut out: Vec<VocabMatch> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for term in terms {
        let needle_core = normalize(term);
        if needle_core.is_empty() || seen.iter().any(|s| s == &needle_core) {
            continue;
        }
        let needle = format!(" {needle_core} ");
        if hay.contains(&needle) {
            seen.push(needle_core);
            out.push(VocabMatch {
                identifier: term.clone(),
                score: 1.0,
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Lowercase, and collapse every non-alphanumeric run to a single space, so
/// `{{the_pipe}}`, `the-pipe`, and `the pipe` all normalise the same. Trimmed.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true; // leading-space suppression
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;

    fn cc(id: &str, label: &str, centroid: Vec<f32>) -> ConceptCentroid {
        ConceptCentroid {
            id: id.into(),
            label: label.into(),
            centroid,
            described_by: Vec::new(),
        }
    }

    fn cfg() -> RoutingConfig {
        RoutingConfig {
            enabled: true,
            abs_threshold: 0.50,
            relative_floor: 0.6,
            max_concepts: 3,
            ..RoutingConfig::default()
        }
    }

    #[test]
    fn routes_the_nearest_concept() {
        let concepts = vec![
            cc("a", "Auth", vec![1.0, 0.0, 0.0]),
            cc("b", "Graph", vec![0.0, 1.0, 0.0]),
            cc("c", "Rag", vec![0.0, 0.0, 1.0]),
        ];
        // Query co-linear with Auth.
        let q = vec![1.0, 0.0, 0.0];
        let set = route("how does auth work", &q, &concepts, vec![], &[], &RelationGraph::empty(), &cfg());
        assert_eq!(set.concepts[0].id, "a", "nearest concept ranks first");
        assert!(set.concepts[0].activated);
        assert!((set.concepts[0].score - 1.0).abs() < 1e-6);
        assert_eq!(set.activated_count, 1, "orthogonal others are off-topic");
        assert!(!set.routed_nothing());
    }

    #[test]
    fn off_topic_query_routes_nothing() {
        // Every centroid is near-orthogonal to the query ⇒ below abs floor.
        let concepts = vec![
            cc("a", "Auth", vec![0.0, 1.0, 0.0]),
            cc("b", "Graph", vec![0.0, 0.0, 1.0]),
        ];
        let q = vec![1.0, 0.0, 0.0];
        let set = route("weather forecast", &q, &concepts, vec![], &[], &RelationGraph::empty(), &cfg());
        assert!(set.routed_nothing(), "nothing clears the absolute floor");
        assert_eq!(set.activated_count, 0);
        assert_eq!(set.chosen_cutoff, cfg().abs_threshold);
        // Suppressed concepts are still reported for the log.
        assert_eq!(set.concepts.len(), 2);
        assert!(set.concepts.iter().all(|c| !c.activated));
    }

    #[test]
    fn a_cluster_activates_several_capped() {
        // Three concepts all near the query, one far. cap = 2.
        let mut c = cfg();
        c.max_concepts = 2;
        let concepts = vec![
            cc("a", "A", vec![1.0, 0.05, 0.0]),
            cc("b", "B", vec![1.0, 0.10, 0.0]),
            cc("c", "C", vec![1.0, 0.15, 0.0]),
            cc("z", "Z", vec![0.0, 0.0, 1.0]),
        ];
        let q = vec![1.0, 0.0, 0.0];
        let set = route("q", &q, &concepts, vec![], &[], &RelationGraph::empty(), &c);
        assert_eq!(set.activated_count, 2, "cluster activates up to the cap");
        assert!(set.concepts[0].activated && set.concepts[1].activated);
        assert!(!set.concepts[2].activated);
    }

    #[test]
    fn empty_concepts_routes_nothing() {
        let set = route("q", &[1.0, 0.0], &[], vec![], &[], &RelationGraph::empty(), &cfg());
        assert!(set.routed_nothing());
        assert!(set.concepts.is_empty());
    }

    #[test]
    fn vocabulary_is_folded_in_without_driving_activation() {
        let concepts = vec![cc("a", "Auth", vec![0.0, 1.0])]; // orthogonal → off-topic
        let q = vec![1.0, 0.0];
        let vocab = match_vocabulary("how does the_pipe work", &["the_pipe".into()], 8);
        let set = route("how does the_pipe work", &q, &concepts, vocab, &[], &RelationGraph::empty(), &cfg());
        assert!(set.routed_nothing(), "vocab match alone does not activate a concept");
        assert_eq!(set.vocabulary.len(), 1);
        assert_eq!(set.vocabulary[0].identifier, "the_pipe");
    }

    #[test]
    fn match_vocabulary_whole_word_only() {
        let terms = vec!["rag".into(), "pipe".into(), "graph".into()];
        let m = match_vocabulary("how does RAG feed the graph", &terms, 8);
        let ids: Vec<&str> = m.iter().map(|v| v.identifier.as_str()).collect();
        assert!(ids.contains(&"rag"), "case-insensitive whole word");
        assert!(ids.contains(&"graph"));
        assert!(!ids.contains(&"pipe"), "absent term not matched");
    }

    #[test]
    fn match_vocabulary_no_substring_false_positive() {
        // "rag" must not match inside "storage".
        let m = match_vocabulary("the storage layer", &["rag".into()], 8);
        assert!(m.is_empty(), "no substring match");
    }

    #[test]
    fn match_vocabulary_normalises_punctuated_terms() {
        // {{the_pipe}} / the-pipe / "the pipe" all normalise to "the pipe".
        let m = match_vocabulary("explain the pipe transport", &["the_pipe".into()], 8);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].identifier, "the_pipe");
    }

    #[test]
    fn match_vocabulary_dedupes_and_caps() {
        let terms = vec!["rag".into(), "rag".into(), "graph".into(), "auth".into()];
        let m = match_vocabulary("rag graph auth", &terms, 2);
        assert_eq!(m.len(), 2, "capped");
        // First two distinct matches in term order.
        assert_eq!(m[0].identifier, "rag");
        assert_eq!(m[1].identifier, "graph");
    }

    #[test]
    fn log_line_marks_activation_and_cutoff() {
        let concepts = vec![cc("a", "Auth", vec![1.0, 0.0]), cc("b", "Graph", vec![0.0, 1.0])];
        let set = route("auth flow", &[1.0, 0.0], &concepts, vec![], &[], &RelationGraph::empty(), &cfg());
        let line = set.log_line();
        assert!(line.contains("★Auth"), "activated concept starred: {line}");
        assert!(line.contains("·Graph"), "suppressed concept dotted: {line}");
        assert!(line.contains("cutoff="));
    }
}
