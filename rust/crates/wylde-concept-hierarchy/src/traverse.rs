//! DAG operations over a projected [`HierGraph`] -- drill-down, multi-parent
//! handling, cycle-safe traversal, the cross-reference walk, and the
//! definitional ancestor-chain accessor (the future injection payload).
//! (definitional-hierarchy plan H0.)
//!
//! Every traversal here is **cycle-safe**: the projection records edges
//! faithfully and never asserts acyclicity, so `parent_concepts` that happen to
//! form a loop (or a diamond) must settle, not spin. Each walk carries a
//! `visited` set -- the read-time analogue of the `visited_best` guard in
//! `wylde_concept_routing::router::spread`. All accessors are pure reads; a
//! query for a node that is not in the graph returns empty, never panics.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::model::{HierGraph, HierNode, NodeId, XRefKind};

/// Which direction a containment walk runs.
#[derive(Clone, Copy)]
enum Dir {
    /// Toward parents (ancestors).
    Up,
    /// Toward children (descendants).
    Down,
}

/// A node reached by the cross-reference walk, with its hop distance from the
/// start (1 = a direct neighbour).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reached {
    pub id: NodeId,
    pub hops: u32,
}

/// What the [`HierGraph::cross_reference_walk`] follows. Containment is
/// undirected here (a leaf reaches its category and vice-versa); each selected
/// cross-reference kind is followed in both directions too -- structural reach,
/// not the activation/inhibition semantics (those are the gated H6 spread step,
/// out of H0 scope).
#[derive(Clone, Debug)]
pub struct WalkOptions {
    /// Follow parent/child containment edges.
    pub follow_containment: bool,
    /// Which typed cross-reference kinds to follow.
    pub follow_xrefs: Vec<XRefKind>,
    /// Hard hop cap (belt-and-braces with the cycle guard).
    pub max_hops: u32,
}

impl WalkOptions {
    /// Follow containment only, to `max_hops`.
    pub fn containment_only(max_hops: u32) -> Self {
        WalkOptions {
            follow_containment: true,
            follow_xrefs: Vec::new(),
            max_hops,
        }
    }

    /// Follow containment + every cross-reference kind, to `max_hops` -- the
    /// full cross-reference reach (plan SS1: "containment + the existing typed
    /// relations").
    pub fn all(max_hops: u32) -> Self {
        WalkOptions {
            follow_containment: true,
            follow_xrefs: vec![
                XRefKind::Names,
                XRefKind::Positive,
                XRefKind::Negative,
                XRefKind::Dependency,
            ],
            max_hops,
        }
    }
}

impl HierGraph {
    /// An id -> node map for the duration of a traversal (the graph is a flat
    /// few-hundred-node table -- the same "small set, build per call" rationale
    /// as the relation adjacency).
    fn index(&self) -> HashMap<&NodeId, &HierNode> {
        self.nodes.iter().map(|n| (&n.id, n)).collect()
    }

    /// The node with this id, if present.
    pub fn node(&self, id: &NodeId) -> Option<&HierNode> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    /// Whether a node with this id is in the graph.
    pub fn contains(&self, id: &NodeId) -> bool {
        self.node(id).is_some()
    }

    /// Drill DOWN: the direct containment children of `id` (empty if `id` is a
    /// leaf or is absent).
    pub fn children_of(&self, id: &NodeId) -> Vec<NodeId> {
        self.node(id)
            .map(|n| n.children.clone())
            .unwrap_or_default()
    }

    /// Drill UP: the direct containment parents of `id` (empty if `id` is a root
    /// or is absent). Multi-parent: all parents are returned.
    pub fn parents_of(&self, id: &NodeId) -> Vec<NodeId> {
        self.node(id).map(|n| n.parents.clone()).unwrap_or_default()
    }

    /// Every node with no parents -- the top of the DAG.
    pub fn roots(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.parents.is_empty())
            .map(|n| n.id.clone())
            .collect()
    }

    /// Every definition-only leaf (no children).
    pub fn leaves(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.is_leaf)
            .map(|n| n.id.clone())
            .collect()
    }

    /// The **definitional ancestor chain** -- the future injection payload (plan
    /// SS3c): `start`, then its primary (first-listed) parent, then *that*
    /// parent's primary parent, up to a root. Returns the ids in nearest-first
    /// order with `start` at index 0, so the caller can render
    /// "workflows -- an N8N graph -- under N8N -- under Wylde" by resolving each
    /// id's definition.
    ///
    /// Multi-parent nodes have more than one path up; this accessor takes the
    /// **deterministic primary path** (the first parent at each step, which is
    /// the first-listed `parent_concepts` / `parent_anchor` entry). Use
    /// [`ancestors`](HierGraph::ancestors) for the full multi-parent set.
    /// Cycle-safe: a loop terminates the chain at the repeat.
    pub fn ancestor_chain(&self, start: &NodeId) -> Vec<NodeId> {
        let idx = self.index();
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        let mut cur = start.clone();
        // Stops on three conditions: an absent/dangling id (`while let` ends),
        // a repeat (cycle guard), or a root (no first parent).
        while let Some(node) = idx.get(&cur) {
            if !visited.insert(cur.clone()) {
                break; // cycle: stop at the repeat
            }
            chain.push(cur.clone());
            match node.parents.first() {
                Some(p) => cur = p.clone(),
                None => break, // reached a root
            }
        }
        chain
    }

    /// The full transitive set of containment ANCESTORS of `start`, in
    /// breadth-first (nearest-first) order, excluding `start`. Follows EVERY
    /// parent of every node, so multi-parent membership is fully covered, and a
    /// shared ancestor appears once. Cycle-safe.
    pub fn ancestors(&self, start: &NodeId) -> Vec<NodeId> {
        self.walk_containment(start, Dir::Up)
    }

    /// The full transitive set of containment DESCENDANTS of `start`, in
    /// breadth-first order, excluding `start`. Cycle-safe.
    pub fn descendants(&self, start: &NodeId) -> Vec<NodeId> {
        self.walk_containment(start, Dir::Down)
    }

    /// Breadth-first containment walk in one direction, cycle-safe, excluding
    /// the start node.
    fn walk_containment(&self, start: &NodeId, dir: Dir) -> Vec<NodeId> {
        let idx = self.index();
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(start.clone());
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        if let Some(n) = idx.get(start) {
            for next in step(n, dir) {
                queue.push_back(next.clone());
            }
        }
        while let Some(cur) = queue.pop_front() {
            if !visited.insert(cur.clone()) {
                continue; // already seen (diamond / cycle)
            }
            out.push(cur.clone());
            if let Some(n) = idx.get(&cur) {
                for next in step(n, dir) {
                    if !visited.contains(next) {
                        queue.push_back(next.clone());
                    }
                }
            }
        }
        out
    }

    /// The **cross-reference walk** -- breadth-first reach over containment (both
    /// directions) PLUS the selected typed cross-reference kinds (both
    /// directions), bounded by `opts.max_hops`, cycle-safe, excluding `start`.
    /// This is the structural substrate the future gated spread step (H6) runs
    /// activation over; here it is a pure reachability query.
    pub fn cross_reference_walk(&self, start: &NodeId, opts: &WalkOptions) -> Vec<Reached> {
        let adj = self.combined_adjacency(opts);
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(start.clone());
        let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
        queue.push_back((start.clone(), 0));
        while let Some((cur, hops)) = queue.pop_front() {
            if hops >= opts.max_hops {
                continue; // hop cap
            }
            if let Some(neighbours) = adj.get(&cur) {
                for ne in neighbours {
                    if visited.insert(ne.clone()) {
                        out.push(Reached {
                            id: ne.clone(),
                            hops: hops + 1,
                        });
                        queue.push_back((ne.clone(), hops + 1));
                    }
                }
            }
        }
        out
    }

    /// Build the undirected neighbour map the cross-reference walk traverses:
    /// containment (from each node's `parents` + `children`, already recorded on
    /// both endpoints) plus each selected cross-reference kind in both
    /// directions.
    fn combined_adjacency(&self, opts: &WalkOptions) -> HashMap<NodeId, Vec<NodeId>> {
        let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        if opts.follow_containment {
            for n in &self.nodes {
                let entry = adj.entry(n.id.clone()).or_default();
                entry.extend(n.parents.iter().cloned());
                entry.extend(n.children.iter().cloned());
            }
        }
        for x in &self.xrefs {
            if opts.follow_xrefs.contains(&x.kind) {
                adj.entry(x.from.clone()).or_default().push(x.to.clone());
                adj.entry(x.to.clone()).or_default().push(x.from.clone());
            }
        }
        adj
    }
}

/// The next-hop ids for a node in a containment direction.
fn step(n: &HierNode, dir: Dir) -> &[NodeId] {
    match dir {
        Dir::Up => &n.parents,
        Dir::Down => &n.children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DefSource, Definition, HierNode, NodeKind, XRef};

    /// A bare concept node with the given parents (children are filled by the
    /// builder below), for hand-built graph fixtures.
    fn node(id: &str, parents: &[&str]) -> HierNode {
        HierNode {
            id: NodeId::concept(id),
            label: id.to_uppercase(),
            definition: Definition {
                text: format!("def of {id}"),
                source: DefSource::InheritedConcept,
            },
            kind: NodeKind::Concept,
            parents: parents.iter().map(|p| NodeId::concept(p)).collect(),
            children: Vec::new(),
            embedding: None,
            is_leaf: true,
        }
    }

    /// Assemble a graph from nodes, deriving the reverse `children` + `is_leaf`
    /// from the declared `parents` (so fixtures only declare edges once).
    fn graph(mut nodes: Vec<HierNode>, xrefs: Vec<XRef>) -> HierGraph {
        let ids: Vec<NodeId> = nodes.iter().map(|n| n.id.clone()).collect();
        for i in 0..nodes.len() {
            let me = ids[i].clone();
            let parents = nodes[i].parents.clone();
            for p in parents {
                if let Some(pi) = ids.iter().position(|x| x == &p) {
                    nodes[pi].children.push(me.clone());
                }
            }
        }
        for n in nodes.iter_mut() {
            n.is_leaf = n.children.is_empty();
        }
        HierGraph { nodes, xrefs }
    }

    #[test]
    fn drill_up_and_down() {
        // wylde -> service -> n8n -> workflows (chain)
        let g = graph(
            vec![
                node("wylde", &[]),
                node("service", &["wylde"]),
                node("n8n", &["service"]),
                node("workflows", &["n8n"]),
            ],
            vec![],
        );
        assert_eq!(
            g.children_of(&NodeId::concept("wylde")),
            vec![NodeId::concept("service")]
        );
        assert_eq!(
            g.parents_of(&NodeId::concept("workflows")),
            vec![NodeId::concept("n8n")]
        );
        assert_eq!(g.roots(), vec![NodeId::concept("wylde")]);
        assert_eq!(g.leaves(), vec![NodeId::concept("workflows")]);
        // Absent node -> empty, never a panic.
        assert!(g.children_of(&NodeId::concept("ghost")).is_empty());
    }

    #[test]
    fn ancestor_chain_is_the_definitional_payload() {
        let g = graph(
            vec![
                node("wylde", &[]),
                node("service", &["wylde"]),
                node("n8n", &["service"]),
                node("workflows", &["n8n"]),
            ],
            vec![],
        );
        // start at index 0, up to the root.
        let chain = g.ancestor_chain(&NodeId::concept("workflows"));
        assert_eq!(
            chain,
            vec![
                NodeId::concept("workflows"),
                NodeId::concept("n8n"),
                NodeId::concept("service"),
                NodeId::concept("wylde"),
            ]
        );
        // A root's chain is just itself.
        assert_eq!(
            g.ancestor_chain(&NodeId::concept("wylde")),
            vec![NodeId::concept("wylde")]
        );
    }

    #[test]
    fn ancestor_chain_takes_the_primary_path_through_a_diamond() {
        // d has two parents [b, c]; both under a. The CHAIN follows the first
        // parent (b); the full ANCESTORS set covers both.
        let g = graph(
            vec![
                node("a", &[]),
                node("b", &["a"]),
                node("c", &["a"]),
                node("d", &["b", "c"]),
            ],
            vec![],
        );
        assert_eq!(
            g.ancestor_chain(&NodeId::concept("d")),
            vec![
                NodeId::concept("d"),
                NodeId::concept("b"),
                NodeId::concept("a")
            ],
            "chain follows the first parent deterministically"
        );
        let mut anc = g.ancestors(&NodeId::concept("d"));
        anc.sort();
        assert_eq!(
            anc,
            vec![
                NodeId::concept("a"),
                NodeId::concept("b"),
                NodeId::concept("c")
            ],
            "full ancestor set covers BOTH parents, a appears once"
        );
    }

    #[test]
    fn descendants_cover_the_subtree_without_duplication() {
        let g = graph(
            vec![
                node("a", &[]),
                node("b", &["a"]),
                node("c", &["a"]),
                node("d", &["b", "c"]), // shared
            ],
            vec![],
        );
        let mut desc = g.descendants(&NodeId::concept("a"));
        desc.sort();
        assert_eq!(
            desc,
            vec![
                NodeId::concept("b"),
                NodeId::concept("c"),
                NodeId::concept("d")
            ],
            "shared descendant d reached once"
        );
    }

    #[test]
    fn traversal_is_cycle_safe() {
        // A 3-cycle: a -> b -> c -> a (each is the other's parent).
        let g = graph(
            vec![node("a", &["c"]), node("b", &["a"]), node("c", &["b"])],
            vec![],
        );
        // ancestors terminates and covers the other two exactly once.
        let mut anc = g.ancestors(&NodeId::concept("a"));
        anc.sort();
        assert_eq!(anc, vec![NodeId::concept("b"), NodeId::concept("c")]);
        // ancestor_chain terminates at the repeat rather than spinning.
        let chain = g.ancestor_chain(&NodeId::concept("a"));
        assert_eq!(chain.len(), 3, "each of a,b,c once, then the cycle stops");
        assert_eq!(chain[0], NodeId::concept("a"));
    }

    #[test]
    fn cross_reference_walk_follows_containment_and_typed_edges() {
        // Containment: a -> b. XRef: b ~Dependency~ x (x is otherwise unrelated).
        let g = graph(
            vec![node("a", &[]), node("b", &["a"]), node("x", &[])],
            vec![XRef {
                from: NodeId::concept("b"),
                to: NodeId::concept("x"),
                kind: XRefKind::Dependency,
            }],
        );

        // Containment-only from a reaches b (1 hop) but NOT x.
        let reached =
            g.cross_reference_walk(&NodeId::concept("a"), &WalkOptions::containment_only(5));
        let ids: Vec<_> = reached.iter().map(|r| r.id.clone()).collect();
        assert!(ids.contains(&NodeId::concept("b")));
        assert!(
            !ids.contains(&NodeId::concept("x")),
            "x is only reachable via the xref"
        );

        // Following all kinds reaches x via b's dependency edge, at 2 hops.
        let reached = g.cross_reference_walk(&NodeId::concept("a"), &WalkOptions::all(5));
        let x = reached
            .iter()
            .find(|r| r.id == NodeId::concept("x"))
            .expect("x reached via xref");
        assert_eq!(x.hops, 2, "a->b (containment) then b->x (dependency)");
    }

    #[test]
    fn cross_reference_walk_respects_the_hop_cap() {
        // chain a -> b -> c -> d (containment).
        let g = graph(
            vec![
                node("a", &[]),
                node("b", &["a"]),
                node("c", &["b"]),
                node("d", &["c"]),
            ],
            vec![],
        );
        let reached =
            g.cross_reference_walk(&NodeId::concept("a"), &WalkOptions::containment_only(2));
        let ids: Vec<_> = reached.iter().map(|r| r.id.clone()).collect();
        assert!(ids.contains(&NodeId::concept("b")), "1 hop");
        assert!(ids.contains(&NodeId::concept("c")), "2 hops");
        assert!(
            !ids.contains(&NodeId::concept("d")),
            "3 hops exceeds the cap of 2"
        );
    }

    #[test]
    fn cross_reference_walk_is_cycle_safe() {
        // Containment cycle a<->b plus a self-referential xref; must terminate.
        let g = graph(
            vec![node("a", &["b"]), node("b", &["a"])],
            vec![XRef {
                from: NodeId::concept("a"),
                to: NodeId::concept("b"),
                kind: XRefKind::Positive,
            }],
        );
        let reached = g.cross_reference_walk(&NodeId::concept("a"), &WalkOptions::all(10));
        // Only b is reachable, exactly once, despite the cycle + redundant xref.
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[0].id, NodeId::concept("b"));
    }
}
