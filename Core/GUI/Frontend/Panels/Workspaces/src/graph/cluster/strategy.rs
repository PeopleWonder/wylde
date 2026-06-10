//! Fold selectors (Slice C-cluster, Build Order §4 `strategy.rs` —
//! "lowest-centrality + deepest selectors").
//!
//! Threshold-driven auto-clustering is the **flat-view fallback** for huge
//! graphs (OQ6): when a workspace has more nodes than the config threshold,
//! fold the coldest corners of the map into single cluster spheres until the
//! visible count is legible again. "Coldest" = **deepest first** (leaf
//! directories before the trunk), ties broken by **lowest centrality**
//! (summed member degree) — the code you're least likely to be looking at
//! folds first, hubs stay visible.

use std::collections::HashSet;

use super::config::ClusterConfig;
use super::precompute::ClusterIndex;

/// Pick which clusters auto-fold. Empty when the graph is small enough to
/// render flat (`node_count ≤ auto_threshold_nodes`). Otherwise folds
/// deepest-then-coldest clusters greedily until the estimated visible count
/// (unfolded nodes + one sphere per folded cluster) reaches the target —
/// or candidates run out (an all-hot graph stays as legible as it can get).
pub fn select_folds(
    node_count: usize,
    index: &ClusterIndex,
    config: &ClusterConfig,
) -> HashSet<String> {
    let mut folds = HashSet::new();
    if node_count <= config.auto_threshold_nodes {
        return folds;
    }

    // Candidates: every assigned cluster big enough to be worth folding.
    let mut candidates: Vec<&String> = index
        .members
        .keys()
        .filter(|id| index.member_count(id) >= config.min_fold_size)
        .collect();
    // Deepest first, then lowest aggregate degree, then id for determinism.
    candidates.sort_by(|a, b| {
        let depth = index.depth.get(*b).cmp(&index.depth.get(*a));
        let degree = index.degree.get(*a).cmp(&index.degree.get(*b));
        depth.then(degree).then(a.cmp(b))
    });

    let mut visible = node_count;
    for id in candidates {
        if visible <= config.target_visible_nodes {
            break;
        }
        let folded_away = index.member_count(id);
        folds.insert(id.clone());
        // Members disappear; one synthetic sphere appears.
        visible = visible.saturating_sub(folded_away) + 1;
    }
    folds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Cluster, Node, NodeKind, Position, WorkspaceGraph};

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

    /// `n` clusters of `size` members each, ids `d<depth>/c<i>`, no edges
    /// (degree 0) unless wired by the caller.
    fn synthetic(clusters: &[(&str, usize)]) -> (WorkspaceGraph, usize) {
        let mut g = WorkspaceGraph::default();
        for (cid, size) in clusters {
            let mut members = Vec::new();
            for i in 0..*size {
                let id = format!("{cid}-n{i}");
                g.nodes.push(node(&id));
                members.push(id);
            }
            g.clusters.push(Cluster {
                id: (*cid).to_owned(),
                member_ids: members,
                parent_breadcrumb: vec![],
                zoom_threshold: 1.0,
            });
        }
        let count = g.nodes.len();
        (g, count)
    }

    fn cfg(auto: usize, target: usize) -> ClusterConfig {
        ClusterConfig {
            auto_threshold_nodes: auto,
            target_visible_nodes: target,
            ..ClusterConfig::default()
        }
    }

    #[test]
    fn small_graph_never_folds() {
        let (g, n) = synthetic(&[("ws/a", 20), ("ws/b", 20)]);
        let idx = ClusterIndex::build(&g);
        assert!(select_folds(n, &idx, &cfg(300, 150)).is_empty());
    }

    #[test]
    fn deepest_folds_first_until_target() {
        // 40 + 40 + 40 = 120 nodes; threshold 100, target 90.
        // Depth order: ws/a/deep/deeper (4) > ws/a/deep (3) > ws/b (2).
        let (g, n) = synthetic(&[("ws/b", 40), ("ws/a/deep", 40), ("ws/a/deep/deeper", 40)]);
        let idx = ClusterIndex::build(&g);
        let folds = select_folds(n, &idx, &cfg(100, 90));
        // Folding the deepest (120 → 81) reaches the target; nothing else folds.
        assert_eq!(folds.len(), 1);
        assert!(folds.contains("ws/a/deep/deeper"));
    }

    #[test]
    fn lowest_centrality_breaks_depth_ties() {
        let (mut g, n) = synthetic(&[("ws/hot", 40), ("ws/cold", 40)]);
        // Heat up ws/hot: internal edges raise its aggregate degree.
        for i in 0..10 {
            g.edges.push(crate::graph::model::Edge {
                src: format!("ws/hot-n{i}"),
                dst: format!("ws/hot-n{}", i + 1),
                rel_type: crate::graph::model::RelType::Calls,
                weight: 1.0,
            });
        }
        let idx = ClusterIndex::build(&g);
        // 80 nodes; threshold 60, target 60: one fold suffices (80 → 41).
        let folds = select_folds(n, &idx, &cfg(60, 60));
        assert_eq!(folds.len(), 1);
        assert!(folds.contains("ws/cold"), "cold cluster folds, hub stays");
    }

    #[test]
    fn tiny_clusters_never_fold() {
        let (g, n) = synthetic(&[("ws/big", 50), ("ws/tiny", 2)]);
        let idx = ClusterIndex::build(&g);
        let folds = select_folds(n, &idx, &cfg(10, 5));
        assert!(!folds.contains("ws/tiny"), "below min_fold_size");
        assert!(folds.contains("ws/big"));
    }

    #[test]
    fn folds_everything_available_when_target_unreachable() {
        let (g, n) = synthetic(&[("ws/a", 30), ("ws/b", 30)]);
        let idx = ClusterIndex::build(&g);
        // Target 1 is unreachable (2 spheres remain) — both fold, no panic.
        let folds = select_folds(n, &idx, &cfg(10, 1));
        assert_eq!(folds.len(), 2);
    }

    #[test]
    fn selection_is_deterministic() {
        let (g, n) = synthetic(&[("ws/a", 40), ("ws/b", 40), ("ws/c", 40)]);
        let idx = ClusterIndex::build(&g);
        let f1 = select_folds(n, &idx, &cfg(100, 90));
        let f2 = select_folds(n, &idx, &cfg(100, 90));
        assert_eq!(f1, f2);
    }
}
