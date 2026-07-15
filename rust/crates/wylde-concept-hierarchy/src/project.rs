//! The projection: `build_view(concepts, anchors, relations) -> HierGraph`
//! (definitional-hierarchy plan H0).
//!
//! **Read-only.** This takes the existing concept / anchor / relation data as
//! input and PROJECTS one unified [`HierGraph`] from it. It never reads or
//! writes a file and never mutates a store -- "project the current stores; layer
//! an empty overlay" (plan SS2.3). Because `parent_concepts` / `parent_anchor` /
//! `described_by` already encode the structure, the DAG is *already drawn*; the
//! projection just folds the three sources into the single node model.
//!
//! ## Why a `ConceptView` and not `Concept`
//!
//! `Concept` lives in Core (`wylde-workspaces`). Depending on Core from an
//! isolated/removable crate would break the very isolation constraint this slice
//! exists to honour, so the projection takes a pure [`ConceptView`] -- the
//! handful of concept fields it actually reads -- and the future H1 bridge (in
//! `wylde-workspaces`, deletable) maps `Concept -> ConceptView`. This mirrors
//! `wylde-concept-routing`, which takes a `ConceptCentroid`, never the Core
//! `Concept`. `Anchor` and `RelationGraph` are NOT Core types (they live in
//! `wylde-shared` and the sibling routing crate), so those are taken directly.
//!
//! ## Dangling endpoints
//!
//! An edge whose endpoint is not a projected node (a `parent_concepts` /
//! `parent_anchor` / `described_by` / relation reference to an id that is not in
//! the input) is **dropped** -- no phantom node is invented (a phantom would
//! have no definition and violate the node invariant) and no edge to a
//! non-existent node ever enters the graph, so traversal is structurally safe.
//! This is the read-time analogue of the `Relation.dangling` rule (retained on
//! disk, excluded from traversal); the on-disk retention itself arrives with the
//! H1 overlay.

use wylde_concept_routing::relations::{NodeRef, RelationGraph, RelationKind};
use wylde_shared::anchor::Anchor;

use crate::config::HierarchyConfig;
use crate::model::{DefSource, Definition, HierGraph, HierNode, NodeId, NodeKind, XRef, XRefKind};
use std::collections::HashMap;

/// The concept fields the projection reads -- a pure, Core-free input view of a
/// `wylde_workspaces::concepts::Concept`. The H1 bridge constructs one of these
/// per concept; H0 tests construct them directly over fixtures.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConceptView {
    /// The concept's stable store id (e.g. `dir:src/graph`, `sem:0007`).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// The concept's description -- the inherited definition source.
    pub description: String,
    /// `CHILD_OF` parents (a DAG; multi-parent). Concept ids.
    #[serde(default)]
    pub parent_concepts: Vec<String>,
    /// `DESCRIBED_BY` -- vocabulary identifiers that name this concept. Become
    /// `names` cross-reference edges, NOT containment edges.
    #[serde(default)]
    pub described_by: Vec<String>,
    /// The centroid embedding (768-dim), if the concept carries one.
    #[serde(default)]
    pub centroid: Option<Vec<f32>>,
}

impl ConceptView {
    /// Construct a view from its parts (test + bridge convenience).
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        ConceptView {
            id: id.into(),
            label: label.into(),
            description: description.into(),
            parent_concepts: Vec::new(),
            described_by: Vec::new(),
            centroid: None,
        }
    }
}

/// Map a relation-graph [`NodeRef`] to a projected [`NodeId`] (the same id
/// scheme `build_view` assigns nodes), so a relation edge can be matched against
/// the projected node table.
fn noderef_to_id(n: &NodeRef) -> NodeId {
    match n {
        NodeRef::Concept { id } => NodeId::concept(id),
        NodeRef::Vocab { identifier } => NodeId::vocab(identifier),
    }
}

/// Map a typed [`RelationKind`] to its cross-reference [`XRefKind`].
fn xref_kind(k: RelationKind) -> XRefKind {
    match k {
        RelationKind::Positive => XRefKind::Positive,
        RelationKind::Negative => XRefKind::Negative,
        RelationKind::Dependency => XRefKind::Dependency,
    }
}

/// Add a containment edge `parent -> child`, recording it on BOTH endpoints
/// (`child.parents` and `parent.children`). Skips it -- as dangling -- when
/// either endpoint is not a projected node, and skips a degenerate self-edge.
/// De-dupes so a doubly-listed parent does not duplicate the edge.
fn add_containment(
    nodes: &mut [HierNode],
    index: &HashMap<NodeId, usize>,
    parent: &NodeId,
    child: &NodeId,
) {
    if parent == child {
        return; // self-edge: degenerate, dropped
    }
    let (Some(&pi), Some(&ci)) = (index.get(parent), index.get(child)) else {
        return; // dangling endpoint: dropped, no phantom node
    };
    if !nodes[ci].parents.contains(parent) {
        nodes[ci].parents.push(parent.clone());
    }
    if !nodes[pi].children.contains(child) {
        nodes[pi].children.push(child.clone());
    }
}

/// De-dupe-push a cross-reference edge.
fn push_xref(xrefs: &mut Vec<XRef>, edge: XRef) {
    if !xrefs.contains(&edge) {
        xrefs.push(edge);
    }
}

/// Project the existing concept / anchor / relation data into one [`HierGraph`].
///
/// * **Concepts** become [`NodeKind::Concept`] nodes (definition inherited from
///   `description`, embedding from `centroid`); `parent_concepts` become
///   concept->concept containment edges.
/// * **Anchors** become [`NodeKind::Vocab`] nodes (definition inherited from
///   `description`, no embedding in H0); `parent_anchor` becomes a vocab->vocab
///   containment edge.
/// * **`described_by`** becomes a [`XRefKind::Names`] cross-reference edge
///   (concept -> vocab), not a containment edge.
/// * **Typed relations** (`concept_relations.json`) become the matching
///   [`XRefKind`] cross-reference edges, skipping any the relation store has
///   already flagged `dangling`.
///
/// Multi-parent is preserved natively and a shared node appears exactly once.
/// Dangling endpoints are dropped (see the module docs). Definitions follow the
/// priority ladder ([`Definition::resolve`]); H0 has no overlay, so a node with
/// an empty backing `description` is flagged [`DefSource::Missing`].
pub fn build_view(
    concepts: &[ConceptView],
    anchors: &[Anchor],
    relations: &RelationGraph,
) -> HierGraph {
    let mut nodes: Vec<HierNode> = Vec::with_capacity(concepts.len() + anchors.len());
    let mut index: HashMap<NodeId, usize> = HashMap::new();

    // -- 1. Nodes. Concepts first, then vocab; each id projected once. --
    for c in concepts {
        let id = NodeId::concept(&c.id);
        if index.contains_key(&id) {
            continue; // duplicate concept id: keep the first, defensively
        }
        let definition =
            Definition::resolve(None, Some(&c.description), DefSource::InheritedConcept);
        index.insert(id.clone(), nodes.len());
        nodes.push(HierNode {
            id,
            label: c.label.clone(),
            definition,
            kind: NodeKind::Concept,
            parents: Vec::new(),
            children: Vec::new(),
            embedding: c.centroid.clone(),
            is_leaf: true,
        });
    }
    for a in anchors {
        let id = NodeId::vocab(&a.identifier);
        if index.contains_key(&id) {
            continue; // a vocab id already taken (or a duplicate anchor)
        }
        let definition =
            Definition::resolve(None, Some(&a.description), DefSource::InheritedAnchor);
        index.insert(id.clone(), nodes.len());
        nodes.push(HierNode {
            id,
            label: a.identifier.clone(),
            definition,
            kind: NodeKind::Vocab,
            parents: Vec::new(),
            children: Vec::new(),
            embedding: None, // vocab nodes embed their definition lazily (H7)
            is_leaf: true,
        });
    }

    // -- 2. Containment edges (parent -> child). Dangling endpoints dropped. --
    for c in concepts {
        let child = NodeId::concept(&c.id);
        for p in &c.parent_concepts {
            add_containment(&mut nodes, &index, &NodeId::concept(p), &child);
        }
    }
    for a in anchors {
        if let Some(p) = &a.parent_anchor {
            add_containment(
                &mut nodes,
                &index,
                &NodeId::vocab(p),
                &NodeId::vocab(&a.identifier),
            );
        }
    }

    // -- 3. Cross-reference edges: `names` (described_by) + typed relations. --
    let mut xrefs: Vec<XRef> = Vec::new();
    for c in concepts {
        let from = NodeId::concept(&c.id);
        for term in &c.described_by {
            let to = NodeId::vocab(term);
            if index.contains_key(&from) && index.contains_key(&to) {
                push_xref(
                    &mut xrefs,
                    XRef {
                        from: from.clone(),
                        to,
                        kind: XRefKind::Names,
                    },
                );
            }
        }
    }
    // Skip relations the store already flagged dangling, AND any whose endpoint
    // is not a projected node.
    for r in relations.relations.iter().filter(|r| !r.dangling) {
        let from = noderef_to_id(&r.from);
        let to = noderef_to_id(&r.to);
        if index.contains_key(&from) && index.contains_key(&to) {
            push_xref(
                &mut xrefs,
                XRef {
                    from,
                    to,
                    kind: xref_kind(r.kind),
                },
            );
        }
    }

    // -- 4. Finalise the leaf flag now that containment is complete. --
    for n in nodes.iter_mut() {
        n.is_leaf = n.children.is_empty();
    }

    HierGraph { nodes, xrefs }
}

/// Toggle-gated projection: returns the view ONLY when the master toggle is on,
/// `None` when it is off. This is how "OFF => inert" is expressed for the pure
/// H0 crate -- a caller that respects the toggle (the H1+ bridge / inject / spread
/// seams) gets *nothing* when the feature is disabled, so today's behaviour is
/// byte-identical. The toggle defaults OFF and fails closed (plan SS4).
pub fn build_view_if_enabled(
    concepts: &[ConceptView],
    anchors: &[Anchor],
    relations: &RelationGraph,
) -> Option<HierGraph> {
    if !HierarchyConfig::current().enabled {
        return None;
    }
    Some(build_view(concepts, anchors, relations))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wylde_concept_routing::relations::Relation;
    use wylde_shared::anchor::{AnchorKind, AnchorScope, AnchorTarget};

    /// A vocab anchor fixture with a description and an optional parent.
    fn anchor(identifier: &str, description: &str, parent: Option<&str>) -> Anchor {
        let mut a = Anchor::new(
            identifier,
            AnchorKind::Concept,
            AnchorTarget::Concept {
                text: identifier.into(),
            },
            AnchorScope::Global,
            description,
        );
        a.parent_anchor = parent.map(|p| p.to_owned());
        a
    }

    #[test]
    fn projects_concepts_and_anchors_into_one_node_model() {
        let mut auth = ConceptView::new("sem:0001", "Auth", "authentication");
        auth.parent_concepts = vec!["sem:0000".into()];
        auth.described_by = vec!["token".into()];
        let root = ConceptView::new("sem:0000", "Security", "the security layer");

        let anchors = vec![anchor("token", "a bearer credential", None)];
        let g = build_view(&[root, auth], &anchors, &RelationGraph::empty());

        // One node per source record, no phantom nodes.
        assert_eq!(g.nodes.len(), 3);
        let by = |id: &NodeId| g.nodes.iter().find(|n| &n.id == id).unwrap();

        let auth_n = by(&NodeId::concept("sem:0001"));
        assert_eq!(auth_n.kind, NodeKind::Concept);
        assert_eq!(auth_n.definition.source, DefSource::InheritedConcept);
        assert_eq!(auth_n.definition.text, "authentication");
        assert_eq!(auth_n.parents, vec![NodeId::concept("sem:0000")]);
        // Auth has no children (Security is its PARENT), so Auth is a leaf.
        assert!(auth_n.children.is_empty());
        assert!(
            auth_n.is_leaf,
            "a node with no children is a definition-only leaf"
        );

        // The parent got the reverse child edge.
        let root_n = by(&NodeId::concept("sem:0000"));
        assert_eq!(root_n.children, vec![NodeId::concept("sem:0001")]);
        assert!(root_n.parents.is_empty(), "Security is a root");

        // The vocab node carries an inherited-anchor definition + no embedding.
        let tok = by(&NodeId::vocab("token"));
        assert_eq!(tok.kind, NodeKind::Vocab);
        assert_eq!(tok.definition.source, DefSource::InheritedAnchor);
        assert!(tok.embedding.is_none());

        // described_by is a `names` xref, NOT a containment edge.
        assert!(g.xrefs.contains(&XRef {
            from: NodeId::concept("sem:0001"),
            to: NodeId::vocab("token"),
            kind: XRefKind::Names,
        }));
        assert!(
            !auth_n.children.contains(&NodeId::vocab("token")),
            "described_by must not become containment"
        );
    }

    #[test]
    fn concept_centroid_becomes_node_embedding() {
        let mut c = ConceptView::new("sem:0001", "Auth", "d");
        c.centroid = Some(vec![0.3, 0.4]);
        let g = build_view(&[c], &[], &RelationGraph::empty());
        assert_eq!(g.nodes[0].embedding, Some(vec![0.3, 0.4]));
    }

    #[test]
    fn multi_parent_diamond_has_no_duplication() {
        // A (root) -> B, C ; both B and C -> D. D is shared.
        let a = ConceptView::new("a", "A", "root");
        let mut b = ConceptView::new("b", "B", "left");
        b.parent_concepts = vec!["a".into()];
        let mut c = ConceptView::new("c", "C", "right");
        c.parent_concepts = vec!["a".into()];
        let mut d = ConceptView::new("d", "D", "shared leaf");
        d.parent_concepts = vec!["b".into(), "c".into()];

        let g = build_view(&[a, b, c, d], &[], &RelationGraph::empty());

        // D appears exactly once, listing BOTH parents.
        let ds: Vec<_> = g
            .nodes
            .iter()
            .filter(|n| n.id == NodeId::concept("d"))
            .collect();
        assert_eq!(ds.len(), 1, "shared node is one node, never duplicated");
        assert_eq!(
            ds[0].parents,
            vec![NodeId::concept("b"), NodeId::concept("c")]
        );
        assert!(ds[0].is_leaf);
    }

    #[test]
    fn typed_relations_become_xrefs_skipping_dangling() {
        let c1 = ConceptView::new("c1", "C1", "one");
        let anchors = vec![
            anchor("nextcloud", "a server", None),
            anchor("ddns", "dynamic dns", None),
        ];

        let mut g_in = RelationGraph::empty();
        g_in.relations.push(Relation::normalized(
            NodeRef::vocab("nextcloud"),
            NodeRef::vocab("ddns"),
            RelationKind::Dependency,
            None,
        ));
        // A dangling relation (store-flagged) must NOT project.
        let mut dead = Relation::normalized(
            NodeRef::concept("c1"),
            NodeRef::vocab("ddns"),
            RelationKind::Positive,
            None,
        );
        dead.dangling = true;
        g_in.relations.push(dead);
        // A relation to a NON-projected node must NOT project either.
        g_in.relations.push(Relation::normalized(
            NodeRef::vocab("nextcloud"),
            NodeRef::vocab("ghost"),
            RelationKind::Positive,
            None,
        ));

        let g = build_view(&[c1], &anchors, &g_in);
        assert!(g.xrefs.contains(&XRef {
            from: NodeId::vocab("nextcloud"),
            to: NodeId::vocab("ddns"),
            kind: XRefKind::Dependency,
        }));
        assert_eq!(
            g.xrefs.len(),
            1,
            "dangling + ghost-endpoint relations dropped"
        );
    }

    #[test]
    fn dangling_containment_parent_is_dropped_no_phantom() {
        // A concept names a parent that does not exist in the input.
        let mut orphan = ConceptView::new("kid", "Kid", "d");
        orphan.parent_concepts = vec!["nonexistent".into()];
        let g = build_view(&[orphan], &[], &RelationGraph::empty());

        assert_eq!(g.nodes.len(), 1, "no phantom parent node invented");
        assert!(
            g.nodes[0].parents.is_empty(),
            "edge to a non-existent parent is dropped"
        );
        assert!(g.nodes[0].is_leaf);
    }

    #[test]
    fn missing_description_flags_needs_definition() {
        let bare = ConceptView::new("c", "C", "   "); // whitespace-only
        let g = build_view(&[bare], &[], &RelationGraph::empty());
        assert!(g.nodes[0].definition.is_missing());
        assert_eq!(g.nodes[0].definition.source, DefSource::Missing);
    }

    #[test]
    fn anchor_parent_becomes_vocab_containment() {
        let anchors = vec![
            anchor("n8n", "automation runtime", None),
            anchor("workflows", "an automation graph", Some("n8n")),
        ];
        let g = build_view(&[], &anchors, &RelationGraph::empty());
        let wf = g
            .nodes
            .iter()
            .find(|n| n.id == NodeId::vocab("workflows"))
            .unwrap();
        assert_eq!(wf.parents, vec![NodeId::vocab("n8n")]);
        let n8n = g
            .nodes
            .iter()
            .find(|n| n.id == NodeId::vocab("n8n"))
            .unwrap();
        assert_eq!(n8n.children, vec![NodeId::vocab("workflows")]);
    }

    #[test]
    fn empty_input_yields_empty_graph() {
        let g = build_view(&[], &[], &RelationGraph::empty());
        assert!(g.nodes.is_empty() && g.xrefs.is_empty());
    }
}
