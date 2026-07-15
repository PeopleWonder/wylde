//! The [`Concept`] data model — the **Concepts** layer of the three-layer
//! semantic map (TBS concept-system thesis §2).
//!
//! A concept is a *system-discovered* semantic theme over the code graph
//! (`authentication`, `persistence/store`, `retrieval`). Unlike a
//! [`crate::anchors`] anchor (a user-curated term), a concept is the triple
//! the thesis §2.3 names:
//!
//!   * **member set** — the code it covers (`Entity`/symbol ids; many-to-many,
//!     so concepts overlap — they are *tags over the graph*, not a partition);
//!   * **centroid** — the mean (re-normalised) embedding of its members, in the
//!     same `nomic-embed-text` 768-dim space the RAG index uses. `None` for the
//!     Phase-0 directory stand-ins (no embeddings yet); filled by Phase-2
//!     semantic clustering. The centroid is what query→concept routing scores
//!     against (a *later*, deferred phase);
//!   * **description** — the human/LLM-authored label + summary.
//!
//! Both hierarchies in the thesis are `CHILD_OF` edges, not nesting. A
//! concept's hierarchy is a **DAG** — [`Concept::parent_concepts`] is a *list*
//! because a theme can sit under several parents (`token authentication` under
//! both `authentication` and `http`).
//!
//! ## Source of truth
//!
//! The JSON store ([`super::store`]) is authoritative — like `anchors.json`,
//! it is offline, encrypted-at-rest, and independent of whether Neo4j is up.
//! The graph projection (`Concept` nodes + `MEMBER`/`CHILD_OF`/`DESCRIBED_BY`
//! edges, [`crate::graph::schema`]) is an *additive* sync target so the graph
//! panel can render concept nodes; it is never the read path for the browse
//! surface or reverse lookup, both of which are pure queries over this store.

use serde::{Deserialize, Serialize};

/// Where a concept came from — the provenance the thesis §7 phase plan turns on
/// (cheap directory stand-ins first, semantic clusters later, manual always
/// allowed). Wire-serialised in snake_case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConceptSource {
    /// Phase-0 stand-in: a labeled `cluster_by_dir` cluster
    /// ([`crate::graph::projection`]). Structural, not semantic; no centroid.
    DirectoryCluster,
    /// Phase-2: an embedding cluster of the chunk vectors. Carries a centroid.
    Embedding,
    /// Hand-authored / hand-edited by the user (curation).
    Manual,
}

impl ConceptSource {
    /// The wire string (mirrors the serde rename) for diagnostics + tests.
    pub fn as_str(self) -> &'static str {
        match self {
            ConceptSource::DirectoryCluster => "directory_cluster",
            ConceptSource::Embedding => "embedding",
            ConceptSource::Manual => "manual",
        }
    }
}

/// One discovered concept. See the module docs for the member-set / centroid /
/// description triple and the source-of-truth rationale.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Concept {
    /// Stable key within a workspace store (a slug; e.g. `dir:src/graph`).
    pub id: String,
    /// Human-readable label (`Graph`, `Persistence / store`).
    pub label: String,
    /// Label + summary — the "what it is".
    pub description: String,
    /// The **member set**: `Entity`/symbol ids this concept covers. Many-to-many
    /// at the set level (a symbol may appear in several concepts).
    #[serde(default)]
    pub members: Vec<String>,
    /// Representative source files the members live in — the "files involved"
    /// the Concepts sub-tab shows. Derived from the members' files; kept
    /// alongside so the browse surface needn't re-walk the graph.
    #[serde(default)]
    pub member_files: Vec<String>,
    /// `CHILD_OF` parents — a DAG (multiple parents allowed; thesis §2.2).
    #[serde(default)]
    pub parent_concepts: Vec<String>,
    /// `DESCRIBED_BY` — vocabulary term identifiers ([`crate::anchors`]) that
    /// name this concept. Empty until a term is linked.
    #[serde(default)]
    pub described_by: Vec<String>,
    /// The centroid embedding (768-dim, re-normalised). `None` for directory
    /// stand-ins; `Some` once semantic clustering computes it. Omitted from
    /// JSON when absent to keep the store compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub centroid: Option<Vec<f32>>,
    /// Provenance (drives the phase upgrade path).
    pub source: ConceptSource,
    /// Epoch seconds at first persist.
    #[serde(default)]
    pub created_at: f64,
    /// Epoch seconds at last persist/edit.
    #[serde(default)]
    pub updated_at: f64,
}

impl Concept {
    /// Build a fresh concept with `created_at`/`updated_at` stamped now.
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        source: ConceptSource,
    ) -> Self {
        let now = wylde_shared::anchor::epoch_now();
        Concept {
            id: id.into(),
            label: label.into(),
            description: description.into(),
            members: Vec::new(),
            member_files: Vec::new(),
            parent_concepts: Vec::new(),
            described_by: Vec::new(),
            centroid: None,
            source,
            created_at: now,
            updated_at: now,
        }
    }

    /// True iff `symbol_id` is in this concept's member set (reverse lookup).
    pub fn has_member(&self, symbol_id: &str) -> bool {
        self.members.iter().any(|m| m == symbol_id)
    }

    /// True iff `file` is among this concept's representative files.
    pub fn touches_file(&self, file: &str) -> bool {
        self.member_files.iter().any(|f| f == file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stamps_timestamps_and_defaults() {
        let c = Concept::new(
            "dir:src/graph",
            "Graph",
            "the graph layer",
            ConceptSource::DirectoryCluster,
        );
        assert_eq!(c.id, "dir:src/graph");
        assert_eq!(c.label, "Graph");
        assert!(c.members.is_empty() && c.member_files.is_empty());
        assert!(c.parent_concepts.is_empty() && c.described_by.is_empty());
        assert!(c.centroid.is_none());
        assert_eq!(c.source, ConceptSource::DirectoryCluster);
        assert!(c.created_at > 0.0 && c.updated_at >= c.created_at);
    }

    #[test]
    fn membership_predicates() {
        let mut c = Concept::new("c1", "C", "d", ConceptSource::Manual);
        c.members = vec!["alpha".into(), "beta".into()];
        c.member_files = vec!["src/a.rs".into()];
        assert!(c.has_member("alpha"));
        assert!(!c.has_member("gamma"));
        assert!(c.touches_file("src/a.rs"));
        assert!(!c.touches_file("src/b.rs"));
    }

    #[test]
    fn centroid_omitted_from_json_when_absent() {
        let c = Concept::new("c1", "C", "d", ConceptSource::DirectoryCluster);
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("centroid").is_none(), "absent centroid omitted: {v}");
        // A directory concept serialises its source as the snake_case wire form.
        assert_eq!(v["source"], "directory_cluster");
    }

    #[test]
    fn centroid_round_trips_when_present() {
        let mut c = Concept::new("c1", "C", "d", ConceptSource::Embedding);
        c.centroid = Some(vec![0.1, 0.2, 0.3]);
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("centroid").is_some());
        let back: Concept = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn source_wire_strings_stable() {
        assert_eq!(
            ConceptSource::DirectoryCluster.as_str(),
            "directory_cluster"
        );
        assert_eq!(ConceptSource::Embedding.as_str(), "embedding");
        assert_eq!(ConceptSource::Manual.as_str(), "manual");
        // serde rename matches as_str.
        for s in [
            ConceptSource::DirectoryCluster,
            ConceptSource::Embedding,
            ConceptSource::Manual,
        ] {
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::Value::String(s.as_str().to_owned())
            );
        }
    }
}
