//! The typed **relation graph** — the data model for concept-routing R1.5a
//! (relation-model addendum `outputs/concept-routing-relation-model.md` §1).
//!
//! Every concept *and* vocabulary word is a node ([`NodeRef`]); the user
//! authors typed edges ([`Relation`]) between them in three kinds
//! ([`RelationKind`]): **Positive** (gentle symmetric co-activation),
//! **Negative** (IS-NOT / soft lateral inhibition, symmetric), and
//! **Dependency** (depends-on, directional in storage but spread both ways at
//! routing time). The whole per-workspace set is a flat [`RelationGraph`]; the
//! spreading-activation engine ([`crate::router::spread`]) builds a cheap
//! adjacency over it once per turn and propagates the flat seed through it.
//!
//! ## Pure types only
//!
//! Like the rest of this crate, the module is pure serde — no I/O, no embed,
//! no service. Persistence (the encrypted `concept_relations.json` store) and
//! the `workspaces.concepts.relations.*` verbs live in the deletable
//! `wylde-workspaces` bridge (`concepts/relations_bridge.rs`), the sibling of
//! `routing_bridge.rs`. Deleting that bridge + this module + the relation verbs
//! reverts routing to pure-seed R1 (the removal test, addendum §1.4).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A node in the relation graph — a concept or a vocabulary anchor.
///
/// Internally-tagged wire shape, matching the `AnchorTarget`/`AnchorScope`
/// idiom (`wylde-shared` `anchor.rs`):
/// `{"node":"concept","id":"…"}` | `{"node":"vocab","identifier":"…"}`.
///
/// `Ord` is derived (variant order then field) purely so symmetric edges can be
/// stored in a canonical orientation — see [`Relation::normalized`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum NodeRef {
    /// A concept, keyed by its workspace-store id (e.g. `dir:src/graph`).
    Concept { id: String },
    /// A vocabulary anchor, keyed by its `{{identifier}}` slug.
    Vocab { identifier: String },
}

impl NodeRef {
    /// Construct a concept node.
    pub fn concept(id: impl Into<String>) -> Self {
        NodeRef::Concept { id: id.into() }
    }
    /// Construct a vocab node.
    pub fn vocab(identifier: impl Into<String>) -> Self {
        NodeRef::Vocab {
            identifier: identifier.into(),
        }
    }
    /// A compact label for the log / the menu (`concept:id` / `vocab:ident`).
    pub fn label(&self) -> String {
        match self {
            NodeRef::Concept { id } => format!("concept:{id}"),
            NodeRef::Vocab { identifier } => format!("vocab:{identifier}"),
        }
    }
}

/// The three user-authored edge kinds (Aaron's locked relation model).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// relates-to / maps-to. Activation co-flows (gentle, decayed). **Symmetric.**
    Positive,
    /// IS-NOT / excludes. Soft multiplicative lateral inhibition. **Symmetric.**
    Negative,
    /// depends-on, **DIRECTIONAL** (`from → to`). Activation spreads BOTH ways
    /// at routing time (forward = its dependencies; backward = its blast
    /// radius) but the stored edge keeps the authored direction for the tree +
    /// labels.
    Dependency,
}

impl RelationKind {
    /// Whether the edge collapses direction in storage (symmetric kinds store a
    /// single canonical orientation so `A⊘B` and `B⊘A` are one record).
    pub fn is_symmetric(self) -> bool {
        matches!(self, RelationKind::Positive | RelationKind::Negative)
    }
    /// Wire string (mirrors the serde rename) for the log / diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::Positive => "positive",
            RelationKind::Negative => "negative",
            RelationKind::Dependency => "dependency",
        }
    }
}

/// One typed edge. Stored once; the engine decides traversal per kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub from: NodeRef,
    pub to: NodeRef,
    pub kind: RelationKind,
    /// Free-text rationale ("DDNS keeps the home IP current"). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Epoch seconds at first persist (harness convention).
    #[serde(default)]
    pub created_at: f64,
    /// **Dangling**: set when a concept recompute dropped an endpoint this edge
    /// points at (Phase-B §4.2). A dangling edge is **retained on disk** and
    /// **surfaced in the relations tree** for the user to re-point or delete,
    /// but **excluded from routing** ([`RelationGraph::adjacency`] /
    /// [`RelationGraph::of_kind`] skip it). `#[serde(default)]` ⇒ migration-free
    /// (an old store with no field loads as `false`). Cleared automatically when
    /// the endpoint resolves again on a later sweep.
    #[serde(default)]
    pub dangling: bool,
}

impl Relation {
    /// Build an edge, **canonicalising symmetric kinds** so `A⊘B` and `B⊘A`
    /// land on the same record (the lexicographically-smaller endpoint becomes
    /// `from`). Dependency keeps the authored direction.
    pub fn normalized(
        from: NodeRef,
        to: NodeRef,
        kind: RelationKind,
        note: Option<String>,
    ) -> Self {
        let (from, to) = if kind.is_symmetric() && to < from {
            (to, from)
        } else {
            (from, to)
        };
        Relation {
            from,
            to,
            kind,
            note,
            created_at: 0.0,
            dangling: false,
        }
    }

    /// Identity for dedupe / removal: two edges are "the same" iff they share
    /// `(from, to, kind)`. Symmetric kinds are compared after normalisation, so
    /// the orientation a caller passed doesn't matter.
    pub fn same_edge(&self, other: &Relation) -> bool {
        self.kind == other.kind && self.from == other.from && self.to == other.to
    }
}

/// The whole per-workspace relation set. Built once per turn from the flat
/// `Vec` (~hundreds of nodes, sparse edges — the same "small set, re-read
/// whole" rationale as `concepts.json`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RelationGraph {
    #[serde(default)]
    pub relations: Vec<Relation>,
}

impl RelationGraph {
    /// An empty graph — the identity for the spread engine (no edge ⇒ pure-seed
    /// R1 behaviour).
    pub fn empty() -> Self {
        RelationGraph::default()
    }

    /// True when there are no edges (the engine short-circuits to identity).
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }

    /// Build the per-turn adjacency: for every node, the edges that touch it.
    /// Symmetric kinds appear under both endpoints; dependency appears under
    /// both too (the spread is bidirectional — addendum §3.1 step 2), so a
    /// single pass over the flat `Vec` makes every node's incident edges
    /// reachable. **Dangling** edges (a dropped endpoint, Phase-B §4.2) are
    /// excluded — the routing engine never sees an edge to a vanished concept.
    /// Rebuilt per turn; never persisted.
    pub fn adjacency(&self) -> HashMap<NodeRef, Vec<&Relation>> {
        let mut adj: HashMap<NodeRef, Vec<&Relation>> = HashMap::new();
        for r in self.relations.iter().filter(|r| !r.dangling) {
            adj.entry(r.from.clone()).or_default().push(r);
            adj.entry(r.to.clone()).or_default().push(r);
        }
        adj
    }

    /// Iterate **non-dangling** edges of one kind (dangling edges are excluded
    /// from routing — Phase-B §4.2).
    pub fn of_kind(&self, kind: RelationKind) -> impl Iterator<Item = &Relation> {
        self.relations
            .iter()
            .filter(move |r| r.kind == kind && !r.dangling)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noderef_wire_shape_is_tagged() {
        let c = NodeRef::concept("dir:src/graph");
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["node"], "concept");
        assert_eq!(v["id"], "dir:src/graph");
        let back: NodeRef = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);

        let v = serde_json::to_value(NodeRef::vocab("nextcloud")).unwrap();
        assert_eq!(v["node"], "vocab");
        assert_eq!(v["identifier"], "nextcloud");
    }

    #[test]
    fn relation_round_trips_with_optional_note() {
        let r = Relation {
            from: NodeRef::vocab("nextcloud"),
            to: NodeRef::vocab("ddns"),
            kind: RelationKind::Dependency,
            note: Some("keeps the home IP current".into()),
            created_at: 1.0,
            dangling: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "dependency");
        let back: Relation = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);

        // An old store with no `dangling` field loads as non-dangling
        // (migration-free serde default).
        let legacy: Relation = serde_json::from_value(serde_json::json!({
            "from": {"node":"vocab","identifier":"a"},
            "to": {"node":"vocab","identifier":"b"},
            "kind": "positive"
        }))
        .unwrap();
        assert!(!legacy.dangling, "absent dangling ⇒ false");

        // A note-less edge omits the key (compact store).
        let bare = Relation::normalized(
            NodeRef::vocab("a"),
            NodeRef::vocab("b"),
            RelationKind::Positive,
            None,
        );
        let v = serde_json::to_value(&bare).unwrap();
        assert!(v.get("note").is_none(), "absent note omitted: {v}");
    }

    #[test]
    fn symmetric_kinds_canonicalise_orientation() {
        // B⊘A and A⊘B normalise to the same record (smaller endpoint as `from`).
        let a = NodeRef::vocab("apple");
        let b = NodeRef::vocab("banana");
        let r1 = Relation::normalized(a.clone(), b.clone(), RelationKind::Negative, None);
        let r2 = Relation::normalized(b.clone(), a.clone(), RelationKind::Negative, None);
        assert_eq!(r1.from, a, "lexicographically-smaller endpoint is `from`");
        assert_eq!(r1.to, b);
        assert!(r1.same_edge(&r2), "both orientations are the same edge");
    }

    #[test]
    fn dependency_keeps_authored_direction() {
        let nc = NodeRef::vocab("nextcloud");
        let ddns = NodeRef::vocab("ddns");
        // nextcloud > ddns lexicographically, but dependency must NOT flip.
        let r = Relation::normalized(nc.clone(), ddns.clone(), RelationKind::Dependency, None);
        assert_eq!(r.from, nc, "dependency direction preserved");
        assert_eq!(r.to, ddns);
    }

    #[test]
    fn adjacency_lists_incident_edges_for_both_endpoints() {
        let g = RelationGraph {
            relations: vec![
                Relation::normalized(
                    NodeRef::vocab("nextcloud"),
                    NodeRef::vocab("ddns"),
                    RelationKind::Dependency,
                    None,
                ),
                Relation::normalized(
                    NodeRef::vocab("nextcloud"),
                    NodeRef::vocab("wylde"),
                    RelationKind::Negative,
                    None,
                ),
            ],
        };
        let adj = g.adjacency();
        assert_eq!(
            adj[&NodeRef::vocab("nextcloud")].len(),
            2,
            "both edges touch nextcloud"
        );
        assert_eq!(adj[&NodeRef::vocab("ddns")].len(), 1);
        assert_eq!(adj[&NodeRef::vocab("wylde")].len(), 1);
    }

    #[test]
    fn empty_graph_predicates() {
        assert!(RelationGraph::empty().is_empty());
        assert!(RelationGraph::empty().adjacency().is_empty());
    }

    #[test]
    fn dangling_edges_excluded_from_routing_but_kept_on_disk() {
        let mut dead = Relation::normalized(
            NodeRef::concept("sem:0001"),
            NodeRef::vocab("ddns"),
            RelationKind::Dependency,
            None,
        );
        dead.dangling = true;
        let live = Relation::normalized(
            NodeRef::vocab("nextcloud"),
            NodeRef::vocab("ddns"),
            RelationKind::Positive,
            None,
        );
        let g = RelationGraph {
            relations: vec![dead, live],
        };
        // Both edges remain on disk (the user can re-point/delete the dangling).
        assert_eq!(g.relations.len(), 2);
        // But routing only sees the live one.
        assert!(
            !g.adjacency().contains_key(&NodeRef::concept("sem:0001")),
            "dangling endpoint absent from adjacency"
        );
        assert_eq!(g.of_kind(RelationKind::Dependency).count(), 0, "dangling skipped");
        assert_eq!(g.of_kind(RelationKind::Positive).count(), 1, "live kept");
        assert_eq!(
            g.adjacency()[&NodeRef::vocab("ddns")].len(),
            1,
            "ddns touched only by the live edge"
        );
    }
}
