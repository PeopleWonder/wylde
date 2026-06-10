//! Clustering behaviour knobs (Slice C-cluster).
//!
//! Build Order §8 convention: every behavioural tunable in exactly one
//! `config.rs`. Visual values (cluster sphere fill, boundary outline, the
//! 300 ms expand animation) come from the [`Theme`]
//! (`module_palette` / `graph_panel.cluster_boundary` /
//! `animations.cluster_expand_in_place`), never from here.
//!
//! [`Theme`]: crate::graph::render::Theme

/// Tunables for threshold-driven auto-clustering (the flat-view fallback for
/// huge graphs, OQ6) and expand-in-place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClusterConfig {
    /// Auto-clustering arms only when the graph has more nodes than this —
    /// small graphs always render flat (folding 40 nodes into 8 spheres
    /// hides information without buying legibility).
    pub auto_threshold_nodes: usize,
    /// The fold selector keeps folding (deepest / lowest-centrality first)
    /// until the estimated visible count (unfolded nodes + one sphere per
    /// folded cluster) drops to this.
    pub target_visible_nodes: usize,
    /// Clusters with fewer members than this never auto-fold — a 2-node
    /// sphere saves nothing and reads as a fat node.
    pub min_fold_size: usize,
    /// Padding (px at zoom 1) around an expanded-in-place cluster's members
    /// when drawing the Theme `cluster_boundary` outline.
    pub boundary_pad_px: f32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            auto_threshold_nodes: 300,
            target_visible_nodes: 150,
            min_fold_size: 3,
            boundary_pad_px: 18.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = ClusterConfig::default();
        assert!(c.target_visible_nodes < c.auto_threshold_nodes);
        assert!(c.min_fold_size >= 2);
        assert!(c.boundary_pad_px > 0.0);
    }
}
