//! The force-directed layout backend — the first [`LayoutBackend`]
//! (Build Order §4). It does the *layout-level* work the physics engine can't:
//!
//! 1. **Dependency depth** — walk from the public entry points (roots) along
//!    CALLS / IMPORTS edges, assigning each node the **longest path from any
//!    root** (Plan v2 §7.5). Computed once at graph load; the physics engine
//!    only ever sees the resulting per-node `y_target`.
//! 2. **Warm start** — seed each node on its depth band, spread horizontally by
//!    sibling index. Starting on-target (vs. random) makes the physics settle
//!    far faster (brief: "phyllotaxis warm-start; faster convergence").
//! 3. **Engine assembly** — hand the bodies (+ y-targets) and edges (+ rest
//!    length) to [`PhysicsEngine::build`].
//!
//! The backend itself is pure + deterministic; all the motion lives in
//! [`super::super::physics`].

use std::collections::HashMap;

use crate::graph::model::{Layout, Position, RelType, WorkspaceGraph};
use crate::graph::physics::{BodyInit, PhysicsConfig, PhysicsEngine};

use super::config::LayoutConfig;
use super::LayoutBackend;

/// The force-directed backend. Holds the layout geometry; the physics knobs are
/// passed at engine-build time so C-settings can tune them independently.
#[derive(Clone, Copy, Debug, Default)]
pub struct ForceDirected {
    pub layout: LayoutConfig,
}

impl ForceDirected {
    pub fn new(layout: LayoutConfig) -> Self {
        Self { layout }
    }

    /// Dependency depth per node, indexed to match `graph.nodes`. Roots
    /// (in-degree 0 over CALLS/IMPORTS) are depth 0; every other node is the
    /// longest path from any root. Cycle-safe: relaxation is capped at `N`
    /// passes so a back-edge can't loop forever.
    pub fn depths(graph: &WorkspaceGraph) -> Vec<u32> {
        let n = graph.nodes.len();
        let index: HashMap<&str, usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.id.as_str(), i))
            .collect();

        // Directed dependency edges (CALLS / IMPORTS) as index pairs.
        let deps: Vec<(usize, usize)> = graph
            .edges
            .iter()
            .filter(|e| matches!(e.rel_type, RelType::Calls | RelType::Imports))
            .filter_map(|e| Some((*index.get(e.src.as_str())?, *index.get(e.dst.as_str())?)))
            .filter(|(s, d)| s != d) // ignore self-loops
            .collect();

        let mut depth = vec![0u32; n];
        if n == 0 {
            return depth;
        }
        // Bellman-Ford-style longest-path relaxation. The longest *simple* path
        // is ≤ N−1, so each assignment is capped at N−1: a back-edge in a cycle
        // can't inflate depth past that bound (and the relaxation converges).
        let cap = n as u32 - 1;
        for _ in 0..n {
            let mut changed = false;
            for &(s, d) in &deps {
                let candidate = (depth[s] + 1).min(cap);
                if candidate > depth[d] {
                    depth[d] = candidate;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        depth
    }

    /// Warm-start positions: each node sits on its depth band (`y_target`),
    /// spread horizontally by its index within that band. Deterministic — the
    /// same graph always warm-starts identically.
    fn warm_positions(&self, graph: &WorkspaceGraph, depths: &[u32]) -> Vec<Position> {
        // Group node indices by depth (sorted by id for determinism within a
        // band).
        let mut by_depth: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut order: Vec<usize> = (0..graph.nodes.len()).collect();
        order.sort_by(|&a, &b| graph.nodes[a].id.cmp(&graph.nodes[b].id));
        for i in order {
            by_depth.entry(depths[i]).or_default().push(i);
        }

        let mut pos = vec![Position::default(); graph.nodes.len()];
        for (&depth, members) in &by_depth {
            let count = members.len();
            let y = self.layout.y_target(depth);
            for (k, &i) in members.iter().enumerate() {
                // Centre the band around x = 0.
                let x = (k as f32 - (count as f32 - 1.0) * 0.5) * self.layout.warm_x_spacing;
                pos[i] = Position { x, y, z: 0.0 };
            }
        }
        pos
    }

    /// Build a ready-to-run [`PhysicsEngine`] from `graph`: warm-start bodies
    /// with their depth-derived y-targets, and edges at the layout rest length.
    pub fn build_engine(&self, graph: &WorkspaceGraph, physics: PhysicsConfig) -> PhysicsEngine {
        let depths = Self::depths(graph);
        let warm = self.warm_positions(graph, &depths);

        let bodies: Vec<BodyInit> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| BodyInit {
                id: node.id.clone(),
                x: warm[i].x,
                y: warm[i].y,
                y_target: self.layout.y_target(depths[i]),
                depth: depths[i],
                pinned: false,
            })
            .collect();

        let rest = self.layout.rest_length();
        let edges: Vec<(String, String, f32)> = graph
            .edges
            .iter()
            .map(|e| (e.src.clone(), e.dst.clone(), rest))
            .collect();

        PhysicsEngine::build(bodies, &edges, physics)
    }

    /// Build an engine that **starts from an explicit `seed` layout** instead of
    /// the depth-banded warm start. Used when resuming physics after an animated
    /// layout swap (Slice C-layout): the tween lands the nodes at the target
    /// positions, then the worker picks up from exactly there rather than
    /// snapping back to a fresh warm start. The y-targets (dependency depth) and
    /// edge rest lengths are still derived from the graph. A node missing from
    /// `seed` falls back to its warm position.
    pub fn build_engine_with_seed(
        &self,
        graph: &WorkspaceGraph,
        physics: PhysicsConfig,
        seed: &Layout,
    ) -> PhysicsEngine {
        let depths = Self::depths(graph);
        let warm = self.warm_positions(graph, &depths);

        let bodies: Vec<BodyInit> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let p = seed.get(&node.id).unwrap_or(warm[i]);
                BodyInit {
                    id: node.id.clone(),
                    x: p.x,
                    y: p.y,
                    y_target: self.layout.y_target(depths[i]),
                    depth: depths[i],
                    pinned: false,
                }
            })
            .collect();

        let rest = self.layout.rest_length();
        let edges: Vec<(String, String, f32)> = graph
            .edges
            .iter()
            .map(|e| (e.src.clone(), e.dst.clone(), rest))
            .collect();

        PhysicsEngine::build(bodies, &edges, physics)
    }
}

impl LayoutBackend for ForceDirected {
    fn name(&self) -> &'static str {
        "force_directed"
    }

    fn compute_positions(&self, graph: &WorkspaceGraph) -> Layout {
        let depths = Self::depths(graph);
        let warm = self.warm_positions(graph, &depths);
        let map = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.id.clone(), warm[i]))
            .collect();
        Layout::from_positions(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Edge, Node, NodeKind};

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

    fn edge(src: &str, dst: &str, rel: RelType) -> Edge {
        Edge {
            src: src.to_owned(),
            dst: dst.to_owned(),
            rel_type: rel,
            weight: 1.0,
        }
    }

    fn depth_of(graph: &WorkspaceGraph, id: &str) -> u32 {
        let depths = ForceDirected::depths(graph);
        let i = graph.nodes.iter().position(|n| n.id == id).unwrap();
        depths[i]
    }

    #[test]
    fn root_is_depth_zero_chain_increments() {
        // root → mid → leaf  (CALLS chain).
        let g = WorkspaceGraph {
            nodes: vec![node("root"), node("mid"), node("leaf")],
            edges: vec![
                edge("root", "mid", RelType::Calls),
                edge("mid", "leaf", RelType::Calls),
            ],
            clusters: vec![],
        };
        assert_eq!(depth_of(&g, "root"), 0);
        assert_eq!(depth_of(&g, "mid"), 1);
        assert_eq!(depth_of(&g, "leaf"), 2);
    }

    #[test]
    fn depth_is_longest_path_not_shortest() {
        // root→a→target and root→target ; longest wins (2, not 1).
        let g = WorkspaceGraph {
            nodes: vec![node("root"), node("a"), node("target")],
            edges: vec![
                edge("root", "a", RelType::Calls),
                edge("a", "target", RelType::Calls),
                edge("root", "target", RelType::Calls),
            ],
            clusters: vec![],
        };
        assert_eq!(depth_of(&g, "target"), 2);
    }

    #[test]
    fn imports_count_inherits_do_not() {
        // INHERITS is not a dependency-depth edge (only CALLS/IMPORTS).
        let g = WorkspaceGraph {
            nodes: vec![node("a"), node("b"), node("c")],
            edges: vec![
                edge("a", "b", RelType::Imports),
                edge("b", "c", RelType::Inherits),
            ],
            clusters: vec![],
        };
        assert_eq!(depth_of(&g, "b"), 1); // via IMPORTS
        assert_eq!(depth_of(&g, "c"), 0); // INHERITS ignored → c stays a root
    }

    #[test]
    fn cycle_does_not_hang() {
        // a→b→a cycle plus a root into it.
        let g = WorkspaceGraph {
            nodes: vec![node("a"), node("b")],
            edges: vec![
                edge("a", "b", RelType::Calls),
                edge("b", "a", RelType::Calls),
            ],
            clusters: vec![],
        };
        let depths = ForceDirected::depths(&g);
        // Bounded (≤ N), finite, no panic.
        assert!(depths.iter().all(|&d| d < g.nodes.len() as u32 + 1));
    }

    #[test]
    fn self_loop_is_ignored() {
        let g = WorkspaceGraph {
            nodes: vec![node("a")],
            edges: vec![edge("a", "a", RelType::Calls)],
            clusters: vec![],
        };
        assert_eq!(depth_of(&g, "a"), 0);
    }

    #[test]
    fn warm_start_places_nodes_on_their_depth_band() {
        let g = WorkspaceGraph {
            nodes: vec![node("root"), node("leaf")],
            edges: vec![edge("root", "leaf", RelType::Calls)],
            clusters: vec![],
        };
        let fd = ForceDirected::default();
        let layout = fd.compute_positions(&g);
        let root_y = layout.get("root").unwrap().y;
        let leaf_y = layout.get("leaf").unwrap().y;
        assert_eq!(root_y, 0.0);
        assert_eq!(leaf_y, 120.0); // one level down
        assert!(leaf_y > root_y, "leaf below root (y grows down)");
    }

    #[test]
    fn seed_is_deterministic() {
        let g = WorkspaceGraph {
            nodes: vec![node("c"), node("a"), node("b")],
            edges: vec![],
            clusters: vec![],
        };
        let fd = ForceDirected::default();
        assert_eq!(fd.compute_positions(&g), fd.compute_positions(&g));
    }

    #[test]
    fn build_engine_carries_nodes_edges_and_targets() {
        let g = WorkspaceGraph {
            nodes: vec![node("root"), node("leaf")],
            edges: vec![edge("root", "leaf", RelType::Calls)],
            clusters: vec![],
        };
        let fd = ForceDirected::default();
        let engine = fd.build_engine(&g, PhysicsConfig::default());
        assert_eq!(engine.len(), 2);
        assert_eq!(fd.name(), "force_directed");
    }

    #[test]
    fn build_engine_with_seed_starts_from_seed_positions() {
        let g = WorkspaceGraph {
            nodes: vec![node("root"), node("leaf")],
            edges: vec![edge("root", "leaf", RelType::Calls)],
            clusters: vec![],
        };
        // A seed that places nodes somewhere the warm start never would.
        let mut map = std::collections::HashMap::new();
        map.insert(
            "root".to_owned(),
            Position {
                x: 500.0,
                y: -300.0,
                z: 0.0,
            },
        );
        map.insert(
            "leaf".to_owned(),
            Position {
                x: -123.0,
                y: 456.0,
                z: 0.0,
            },
        );
        let seed = Layout::from_positions(map);
        let fd = ForceDirected::default();
        let engine = fd.build_engine_with_seed(&g, PhysicsConfig::default(), &seed);
        // The worker's first latched frame reflects the seed, not a warm start.
        let frame = engine.snapshot();
        let root = frame
            .positions
            .iter()
            .find(|(id, _)| &**id == "root")
            .unwrap()
            .1;
        assert!((root.x - 500.0).abs() < 1e-3 && (root.y + 300.0).abs() < 1e-3);
    }
}
