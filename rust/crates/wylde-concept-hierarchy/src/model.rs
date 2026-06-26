//! The projected node model -- the locked node shape
//! `{ id, label, definition, children, parents }` plus the provenance a
//! projection needs (definitional-hierarchy plan SS2.1).
//!
//! Pure serde types only -- no I/O, no embed, no service, the same discipline
//! as `wylde_concept_routing::relations`. The DAG is **id-linked and flat**:
//! [`HierGraph`] is a flat node table with cross-reference edges alongside it,
//! NEVER a nested dict. The nested drill-down is *rendered* from the flat graph
//! by [`crate::traverse`] -- a literal nested tree is a strict tree that
//! duplicates shared subtrees and cannot express the multi-parent membership
//! the model is built for (plan SS2.1).

use serde::{Deserialize, Serialize};

/// A stable, provenance-encoding node id. Built FROM ids that already carry
/// over across recompute (concept store ids are never re-used; anchors are
/// keyed by their identifier), so authored structure layered on top in later
/// slices re-binds to the same node after every recompute (plan SS2.3).
///
/// The wire form is `"<prefix>:<source-id>"`, split on the **first** colon so a
/// source id that itself contains colons round-trips:
///   * `concept:dir:src/graph` | `concept:sem:0007` -- a concept node
///   * `vocab:nextcloud`                             -- a vocabulary anchor node
///   * `node:0003`                                   -- an overlay-only node (H1+)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// The id for a concept node, from its workspace-store id.
    pub fn concept(id: &str) -> Self {
        NodeId(format!("concept:{id}"))
    }

    /// The id for a vocabulary node, from its anchor identifier.
    pub fn vocab(identifier: &str) -> Self {
        NodeId(format!("vocab:{identifier}"))
    }

    /// The raw wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `(prefix, source-id)` split on the first colon, if well-formed.
    /// `concept:dir:src/graph` -> `("concept", "dir:src/graph")`.
    pub fn split(&self) -> Option<(&str, &str)> {
        self.0.split_once(':')
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a node *is*. H0 projects only [`Concept`](NodeKind::Concept) and
/// [`Vocab`](NodeKind::Vocab) nodes from the existing stores; the
/// [`Authored`](NodeKind::Authored) kind is carried for forward-compatibility
/// with the H1 overlay (a net-new node the source stores cannot express).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A discovered concept (`concepts.json`).
    Concept,
    /// A vocabulary anchor (`anchors.json`).
    Vocab,
    /// An overlay-only authored node (H1+); never produced by H0's projection.
    Authored,
}

/// Where a node's [`Definition`] came from -- the priority ladder of plan SS1
/// ("authored -> inherited description -> LLM draft -> flagged Missing").
/// H0 has no overlay and no LLM pass, so it only ever produces
/// [`InheritedConcept`](DefSource::InheritedConcept),
/// [`InheritedAnchor`](DefSource::InheritedAnchor) or
/// [`Missing`](DefSource::Missing); the [`Authored`](DefSource::Authored) and
/// [`LlmDraft`](DefSource::LlmDraft) rungs are carried so the ladder is the
/// real, complete one the H1 overlay slots into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefSource {
    /// The user authored this definition explicitly (overlay; highest priority).
    Authored,
    /// Inherited from the backing anchor's `description`.
    InheritedAnchor,
    /// Inherited from the backing concept's `description`.
    InheritedConcept,
    /// An LLM-drafted fallback awaiting confirmation (deferred; flagged so the
    /// UI can prompt).
    LlmDraft,
    /// No definition anywhere -- the node is flagged "needs definition". This is
    /// how the "every node has a definition" invariant *surfaces* rather than
    /// silently passing (plan SS1, SS3).
    Missing,
}

/// A node's definition + where it was sourced from. `text` is empty iff
/// `source` is [`DefSource::Missing`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub text: String,
    pub source: DefSource,
}

impl Definition {
    /// The empty / "needs definition" sentinel.
    pub fn missing() -> Self {
        Definition {
            text: String::new(),
            source: DefSource::Missing,
        }
    }

    /// True when no definition exists (the node is browse-only; never injected
    /// -- plan SS3 invariant surfacing).
    pub fn is_missing(&self) -> bool {
        matches!(self.source, DefSource::Missing)
    }

    /// Resolve a definition by the locked priority ladder (plan SS1):
    /// `authored` (overlay) wins over `inherited` (the backing record's
    /// description) wins over [`Missing`](DefSource::Missing). A
    /// whitespace-only string counts as absent at each rung, so an empty
    /// authored override correctly falls through to the inherited description.
    ///
    /// `inherited_kind` says which inherited rung the backing record is --
    /// [`InheritedConcept`](DefSource::InheritedConcept) for a concept,
    /// [`InheritedAnchor`](DefSource::InheritedAnchor) for an anchor.
    ///
    /// H0 always passes `authored = None` (no overlay yet); the parameter is
    /// real so the ladder is exercised and correct for when H1 layers the
    /// overlay on top.
    pub fn resolve(
        authored: Option<&str>,
        inherited: Option<&str>,
        inherited_kind: DefSource,
    ) -> Definition {
        if let Some(a) = authored {
            if !a.trim().is_empty() {
                return Definition {
                    text: a.to_owned(),
                    source: DefSource::Authored,
                };
            }
        }
        if let Some(i) = inherited {
            if !i.trim().is_empty() {
                return Definition {
                    text: i.to_owned(),
                    source: inherited_kind,
                };
            }
        }
        Definition::missing()
    }
}

/// One node of the projected DAG -- the locked shape `{ id, label, definition,
/// children, parents }` plus provenance (`kind`, `embedding`, `is_leaf`).
///
/// `parents` / `children` are **containment** edges only (parent/child), held
/// natively multi-parent. Typed cross-reference edges (`names`, and the
/// `Positive`/`Negative`/`Dependency` relations) are NOT on the node -- they
/// live in [`HierGraph::xrefs`] as a separate adjacency the hierarchy crate
/// owns, keeping `concept_relations.json`'s wire shape frozen (plan SS1, OQ-6
/// default). The node stays exactly the locked containment shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HierNode {
    pub id: NodeId,
    pub label: String,
    pub definition: Definition,
    pub kind: NodeKind,
    /// Containment parents -- multi-parent, native (a theme can sit under
    /// several parents). No subtree duplication: a shared node is ONE node
    /// listing all its parents.
    pub parents: Vec<NodeId>,
    /// Containment children. Empty for a definition-only leaf.
    pub children: Vec<NodeId>,
    /// The concept centroid (768-dim), or `None` for vocab nodes (lazily
    /// embedded in H7). Carried so a future fuzzy-entry step can resolve a
    /// query vector to a node.
    pub embedding: Option<Vec<f32>>,
    /// `children.is_empty()` -- a definition-only leaf. Recomputed by the
    /// projection after all containment edges are assembled.
    pub is_leaf: bool,
}

/// A typed cross-reference edge -- the non-containment links the cross-reference
/// walk traverses alongside parent/child (plan SS1: "containment + the existing
/// typed relations").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XRefKind {
    /// `Concept.described_by` -- a concept names itself via a vocab term. NOT a
    /// containment edge (plan SS1 table; OQ-2 default keeps them two nodes).
    Names,
    /// A `Positive` (relates-to) typed relation.
    Positive,
    /// A `Negative` (IS-NOT / excludes) typed relation.
    Negative,
    /// A `Dependency` (depends-on) typed relation.
    Dependency,
}

/// One cross-reference edge between two projected nodes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct XRef {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: XRefKind,
}

/// The whole projected view: a flat, id-linked node table plus the typed
/// cross-reference adjacency. NEVER nested on disk; the nested drill-down is
/// rendered from this by [`crate::traverse`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HierGraph {
    pub nodes: Vec<HierNode>,
    #[serde(default)]
    pub xrefs: Vec<XRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_constructors_and_split() {
        assert_eq!(NodeId::concept("sem:0007").as_str(), "concept:sem:0007");
        assert_eq!(NodeId::vocab("nextcloud").as_str(), "vocab:nextcloud");
        // Split is on the FIRST colon, so a colon-bearing concept id survives.
        assert_eq!(
            NodeId::concept("dir:src/graph").split(),
            Some(("concept", "dir:src/graph"))
        );
        assert_eq!(NodeId::vocab("nextcloud").split(), Some(("vocab", "nextcloud")));
    }

    #[test]
    fn definition_priority_authored_beats_inherited_beats_missing() {
        // Authored wins.
        let d = Definition::resolve(Some("hand-written"), Some("from record"), DefSource::InheritedConcept);
        assert_eq!(d.source, DefSource::Authored);
        assert_eq!(d.text, "hand-written");
        // No authored -> inherited (with the kind the caller named).
        let d = Definition::resolve(None, Some("from record"), DefSource::InheritedAnchor);
        assert_eq!(d.source, DefSource::InheritedAnchor);
        assert_eq!(d.text, "from record");
        // Nothing -> Missing, empty text.
        let d = Definition::resolve(None, None, DefSource::InheritedConcept);
        assert!(d.is_missing());
        assert!(d.text.is_empty());
    }

    #[test]
    fn definition_whitespace_only_counts_as_absent() {
        // An empty/whitespace authored override falls through to inherited.
        let d = Definition::resolve(Some("   "), Some("real"), DefSource::InheritedConcept);
        assert_eq!(d.source, DefSource::InheritedConcept);
        assert_eq!(d.text, "real");
        // A whitespace-only inherited description flags Missing, not a blank def.
        let d = Definition::resolve(None, Some("\n\t "), DefSource::InheritedAnchor);
        assert!(d.is_missing());
    }

    #[test]
    fn model_types_round_trip_through_json() {
        let g = HierGraph {
            nodes: vec![HierNode {
                id: NodeId::concept("sem:0001"),
                label: "Auth".into(),
                definition: Definition {
                    text: "authentication".into(),
                    source: DefSource::InheritedConcept,
                },
                kind: NodeKind::Concept,
                parents: vec![NodeId::concept("sem:0000")],
                children: vec![NodeId::vocab("token")],
                embedding: Some(vec![0.1, 0.2]),
                is_leaf: false,
            }],
            xrefs: vec![XRef {
                from: NodeId::concept("sem:0001"),
                to: NodeId::vocab("token"),
                kind: XRefKind::Names,
            }],
        };
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["nodes"][0]["kind"], "concept");
        assert_eq!(v["nodes"][0]["definition"]["source"], "inherited_concept");
        assert_eq!(v["xrefs"][0]["kind"], "names");
        let back: HierGraph = serde_json::from_value(v).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn xrefs_default_when_absent() {
        // A graph serialised without xrefs (older shape) still loads.
        let g: HierGraph = serde_json::from_value(serde_json::json!({ "nodes": [] })).unwrap();
        assert!(g.nodes.is_empty() && g.xrefs.is_empty());
    }
}
