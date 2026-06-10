//! `WorkspaceGraph` — the top-level graph state the `workspaces.graph` verb
//! returns, deserialised from the verb reply (`{nodes, edges, clusters}`).
//!
//! This module also owns the *scaffold layout* — a deterministic placement of
//! nodes in model space. C-scaffold has **no physics and no real layout
//! engine** (those are Slices C-physics / C-layout): the verb's positions are
//! all the origin in v1, so we spread nodes on a phyllotaxis (sunflower)
//! spiral purely so the data → screen path is visible and stable. Replace
//! `scaffold_layout` wholesale when C-physics lands; nothing else depends on
//! how positions are chosen.
//!
//! Canonical home for `WorkspaceGraph` (Build Order Appendix B → GUI
//! Workspaces · `graph/model/workspace_graph.rs`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{Cluster, Edge, Node, Position};

/// The top-level graph state. Field set + order mirror the service so this
/// deserialises straight off `workspaces.graph`'s reply.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceGraph {
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub clusters: Vec<Cluster>,
}

/// Node id → model-space position. The renderer/viewport projects these to
/// screen pixels; hit-testing reads them back. A thin wrapper so callers
/// don't pass a bare `HashMap` around.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    positions: HashMap<String, Position>,
}

impl Layout {
    /// Build a layout from an explicit id → position map. C-scaffold only ever
    /// produced layouts via [`WorkspaceGraph::scaffold_layout`]; C-physics needs
    /// to hand the physics worker's settled positions back as a `Layout`, so
    /// this is the constructor the force-directed backend / worker bridge use.
    pub fn from_positions(positions: HashMap<String, Position>) -> Self {
        Layout { positions }
    }

    pub fn get(&self, id: &str) -> Option<Position> {
        self.positions.get(id).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Position)> {
        self.positions.iter()
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

/// Spacing constant for the scaffold spiral (model units between successive
/// ring steps). Lives here, not in a `config.rs` — there is no layout config
/// surface until C-layout, and this is throwaway placement.
const SCAFFOLD_SPIRAL_SPACING: f32 = 34.0;

/// The golden angle (137.507°) in radians — the phyllotaxis constant that
/// gives an even, non-clumping spread.
const GOLDEN_ANGLE: f32 = 2.399_963_2;

impl WorkspaceGraph {
    /// Parse a `workspaces.graph` reply value into a `WorkspaceGraph`.
    pub fn from_value(v: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(v)
    }

    pub fn node_by_id(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn cluster_by_id(&self, id: &str) -> Option<&Cluster> {
        self.clusters.iter().find(|c| c.id == id)
    }

    /// Deterministic placeholder layout (see module docs). Nodes are sorted
    /// by id so the same graph always lays out the same way, then placed on a
    /// sunflower spiral centred on the model origin. **Not** force-directed;
    /// replaced entirely by C-physics / C-layout.
    pub fn scaffold_layout(&self) -> Layout {
        let mut ids: Vec<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();

        let mut positions = HashMap::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            let idx = i as f32;
            let angle = idx * GOLDEN_ANGLE;
            let radius = SCAFFOLD_SPIRAL_SPACING * idx.sqrt();
            positions.insert(
                (*id).to_owned(),
                Position {
                    x: radius * angle.cos(),
                    y: radius * angle.sin(),
                    z: 0.0,
                },
            );
        }
        Layout { positions }
    }

    /// Bounding box of the scaffold layout in model space as
    /// `(min_x, min_y, max_x, max_y)`. Used by the viewport to fit the graph
    /// to the canvas on first load. `None` for an empty graph.
    pub fn model_bounds(&self, layout: &Layout) -> Option<(f32, f32, f32, f32)> {
        let mut it = layout.iter();
        let (_, first) = it.next()?;
        let mut bb = (first.x, first.y, first.x, first.y);
        for (_, p) in it {
            bb.0 = bb.0.min(p.x);
            bb.1 = bb.1.min(p.y);
            bb.2 = bb.2.max(p.x);
            bb.3 = bb.3.max(p.y);
        }
        Some(bb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{NodeKind, RelType};
    use serde_json::json;

    fn node(id: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            file: format!("src/{id}.rs"),
            line: 0,
            position: Position::default(),
            style: Default::default(),
        }
    }

    #[test]
    fn deserialises_full_graph_reply() {
        let v = json!({
            "nodes": [
                { "id": "alpha", "kind": "Function", "name": "alpha", "file": "src/a.rs" },
                { "id": "beta", "kind": "Module", "name": "beta", "file": "src/b.rs" }
            ],
            "edges": [
                { "src": "alpha", "dst": "beta", "rel_type": "CALLS", "weight": 1.0 }
            ],
            "clusters": [
                { "id": "src", "member_ids": ["alpha", "beta"], "parent_breadcrumb": ["src"], "zoom_threshold": 1.0 }
            ]
        });
        let g = WorkspaceGraph::from_value(v).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].rel_type, RelType::Calls);
        assert_eq!(g.clusters.len(), 1);
        assert_eq!(g.node_by_id("beta").unwrap().kind, NodeKind::Module);
    }

    #[test]
    fn empty_reply_is_empty_graph() {
        let g = WorkspaceGraph::from_value(json!({})).unwrap();
        assert!(g.nodes.is_empty() && g.edges.is_empty() && g.clusters.is_empty());
        assert!(g.scaffold_layout().is_empty());
        assert_eq!(g.model_bounds(&g.scaffold_layout()), None);
    }

    #[test]
    fn scaffold_layout_is_deterministic_and_total() {
        let g = WorkspaceGraph {
            nodes: vec![node("c"), node("a"), node("b")],
            ..Default::default()
        };
        let l1 = g.scaffold_layout();
        let l2 = g.scaffold_layout();
        assert_eq!(l1, l2, "same graph → same layout");
        assert_eq!(l1.len(), 3, "every node placed");
        for id in ["a", "b", "c"] {
            assert!(l1.get(id).is_some(), "{id} placed");
            assert_eq!(l1.get(id).unwrap().z, 0.0, "z forced to 0 in v1");
        }
        // First node (sorted: "a") sits at the spiral origin.
        let a = l1.get("a").unwrap();
        assert!(a.x.abs() < 1e-4 && a.y.abs() < 1e-4);
    }

    #[test]
    fn layout_order_independent_of_node_order() {
        let g1 = WorkspaceGraph {
            nodes: vec![node("a"), node("b"), node("c")],
            ..Default::default()
        };
        let g2 = WorkspaceGraph {
            nodes: vec![node("c"), node("b"), node("a")],
            ..Default::default()
        };
        assert_eq!(g1.scaffold_layout(), g2.scaffold_layout());
    }

    #[test]
    fn model_bounds_covers_all_nodes() {
        let g = WorkspaceGraph {
            nodes: (0..20).map(|i| node(&format!("n{i:02}"))).collect(),
            ..Default::default()
        };
        let layout = g.scaffold_layout();
        let (min_x, min_y, max_x, max_y) = g.model_bounds(&layout).unwrap();
        assert!(min_x <= max_x && min_y <= max_y);
        for (_, p) in layout.iter() {
            assert!(p.x >= min_x && p.x <= max_x && p.y >= min_y && p.y <= max_y);
        }
    }
}
