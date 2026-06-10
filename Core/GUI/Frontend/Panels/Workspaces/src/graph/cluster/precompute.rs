//! One-time cluster assignment (Slice C-cluster, Build Order §4
//! `precompute.rs` — "one-time assignment at ingest").
//!
//! Built once per graph load from the wire clusters (Slice B: one per file
//! parent directory) and consulted every frame by the display-graph
//! transform. Pure data — no gpui, no Theme.

use std::collections::HashMap;

use crate::graph::model::{Layout, Position, WorkspaceGraph};

/// Node→cluster assignment plus per-cluster aggregates, computed once per
/// graph load.
#[derive(Clone, Debug, Default)]
pub struct ClusterIndex {
    /// node id → owning cluster id. Nodes outside every wire cluster are
    /// absent (they never fold).
    pub assignment: HashMap<String, String>,
    /// cluster id → member node ids, validated against the node set (wire
    /// member ids that don't resolve to real nodes are dropped).
    pub members: HashMap<String, Vec<String>>,
    /// cluster id → summed degree of its members (centrality proxy for the
    /// fold strategy; lower = colder = folds first).
    pub degree: HashMap<String, usize>,
    /// cluster id → nesting depth (path-segment count of the id; deeper
    /// folds first).
    pub depth: HashMap<String, usize>,
}

impl ClusterIndex {
    /// Build the index from the loaded graph. A node claimed by multiple wire
    /// clusters keeps the first claim (wire order is deterministic).
    pub fn build(graph: &WorkspaceGraph) -> ClusterIndex {
        let node_ids: std::collections::HashSet<&str> =
            graph.nodes.iter().map(|n| n.id.as_str()).collect();
        let mut node_degree: HashMap<&str, usize> = HashMap::new();
        for e in &graph.edges {
            *node_degree.entry(e.src.as_str()).or_default() += 1;
            *node_degree.entry(e.dst.as_str()).or_default() += 1;
        }

        let mut idx = ClusterIndex::default();
        for c in &graph.clusters {
            let mut members = Vec::new();
            let mut deg = 0usize;
            for m in &c.member_ids {
                if !node_ids.contains(m.as_str()) {
                    continue; // wire id with no node — not renderable
                }
                if idx.assignment.contains_key(m) {
                    continue; // first claim wins
                }
                idx.assignment.insert(m.clone(), c.id.clone());
                deg += node_degree.get(m.as_str()).copied().unwrap_or(0);
                members.push(m.clone());
            }
            if members.is_empty() {
                continue;
            }
            idx.members.insert(c.id.clone(), members);
            idx.degree.insert(c.id.clone(), deg);
            idx.depth.insert(c.id.clone(), path_depth(&c.id));
        }
        idx
    }

    /// The mean position of a cluster's members, or `None` when no member is
    /// placed. Folded members collapse onto this point; the synthetic cluster
    /// sphere renders here.
    pub fn centroid(&self, cluster_id: &str, layout: &Layout) -> Option<Position> {
        let members = self.members.get(cluster_id)?;
        let mut sum = (0.0f32, 0.0f32);
        let mut n = 0usize;
        for m in members {
            if let Some(p) = layout.get(m) {
                sum.0 += p.x;
                sum.1 += p.y;
                n += 1;
            }
        }
        if n == 0 {
            return None;
        }
        Some(Position {
            x: sum.0 / n as f32,
            y: sum.1 / n as f32,
            z: 0.0,
        })
    }

    pub fn member_count(&self, cluster_id: &str) -> usize {
        self.members.get(cluster_id).map_or(0, Vec::len)
    }
}

/// Nesting depth of a path-shaped cluster id (`C:/ws/src/graph` → 4 segments).
fn path_depth(id: &str) -> usize {
    id.split(['/', '\\']).filter(|s| !s.is_empty()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Cluster, Edge, Node, NodeKind, RelType};

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

    fn edge(src: &str, dst: &str) -> Edge {
        Edge {
            src: src.to_owned(),
            dst: dst.to_owned(),
            rel_type: RelType::Calls,
            weight: 1.0,
        }
    }

    fn cluster(id: &str, members: &[&str]) -> Cluster {
        Cluster {
            id: id.to_owned(),
            member_ids: members.iter().map(|s| (*s).to_owned()).collect(),
            parent_breadcrumb: vec![],
            zoom_threshold: 1.0,
        }
    }

    fn graph() -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![node("a"), node("b"), node("c"), node("loose")],
            edges: vec![edge("a", "b"), edge("a", "c"), edge("a", "loose")],
            clusters: vec![
                cluster("ws/src", &["a", "b", "ghost"]),
                cluster("ws/src/deep", &["c"]),
            ],
        }
    }

    #[test]
    fn build_assigns_validates_and_aggregates() {
        let idx = ClusterIndex::build(&graph());
        assert_eq!(idx.assignment.get("a").unwrap(), "ws/src");
        assert_eq!(idx.assignment.get("c").unwrap(), "ws/src/deep");
        assert!(
            !idx.assignment.contains_key("ghost"),
            "wire id without a node dropped"
        );
        assert!(
            !idx.assignment.contains_key("loose"),
            "unclustered node unassigned"
        );
        // a (deg 3) + b (deg 1) = 4.
        assert_eq!(idx.degree["ws/src"], 4);
        assert_eq!(idx.degree["ws/src/deep"], 1);
        assert_eq!(idx.depth["ws/src"], 2);
        assert_eq!(idx.depth["ws/src/deep"], 3);
        assert_eq!(idx.member_count("ws/src"), 2);
    }

    #[test]
    fn first_claim_wins_on_double_membership() {
        let mut g = graph();
        g.clusters.push(cluster("ws/again", &["a"]));
        let idx = ClusterIndex::build(&g);
        assert_eq!(idx.assignment.get("a").unwrap(), "ws/src");
        assert!(
            !idx.members.contains_key("ws/again"),
            "cluster left with no members is dropped"
        );
    }

    #[test]
    fn centroid_averages_placed_members() {
        let g = graph();
        let idx = ClusterIndex::build(&g);
        let mut pos = HashMap::new();
        pos.insert(
            "a".to_owned(),
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        pos.insert(
            "b".to_owned(),
            Position {
                x: 10.0,
                y: 20.0,
                z: 0.0,
            },
        );
        let layout = Layout::from_positions(pos);
        let c = idx.centroid("ws/src", &layout).unwrap();
        assert_eq!((c.x, c.y), (5.0, 10.0));
        assert!(
            idx.centroid("ws/src/deep", &layout).is_none(),
            "no member placed"
        );
        assert!(idx.centroid("nope", &layout).is_none());
    }
}
