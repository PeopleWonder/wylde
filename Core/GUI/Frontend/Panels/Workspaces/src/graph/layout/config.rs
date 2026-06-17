//! Layout-level tunables — the **single source of truth** for *where the bands
//! are*, kept separate from the force model ([`super::super::physics::config`]).
//!
//! These shape the target geometry the physics engine relaxes toward:
//! how far apart dependency levels sit (`level_spacing`), which way depth grows
//! on screen (`y_axis_down`), and the spring rest length (a multiple of the
//! level spacing). C-settings later exposes `level_spacing` as a user knob.

/// Layout geometry knobs. `Copy` so a backend holds its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutConfig {
    /// Vertical distance (model px) between successive dependency levels — a
    /// node at depth `d` targets `y = d · level_spacing` (Plan v2 §7.5
    /// default 120 px).
    pub level_spacing: f32,
    /// If true (default) depth grows **downward**: roots (depth 0) at the top,
    /// leaves at the bottom — matching screen-space y. A future setting can
    /// flip this.
    pub y_axis_down: bool,
    /// Spring rest length as a multiple of `level_spacing`. Default 1.2 (Plan
    /// v2 §7.5) keeps connected nodes a little farther apart than one level.
    pub rest_length_multiplier: f32,
    /// Horizontal spacing (model px) between sibling nodes in the same depth
    /// band at warm start. Only seeds the initial layout — physics spreads them
    /// the rest of the way.
    pub warm_x_spacing: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            level_spacing: 120.0,
            y_axis_down: true,
            rest_length_multiplier: 1.2,
            warm_x_spacing: 72.0,
        }
    }
}

impl LayoutConfig {
    /// The layout geometry to use for a body of `node_count` nodes.
    ///
    /// Small/medium graphs keep the locked default. Past
    /// [`super::super::physics::config::LARGE_GRAPH_THRESHOLD`] we shorten the
    /// spring rest length (visual-polish G4) so a dense 10 k-node graph reels
    /// its edges in tighter instead of sprawling — paired with the calmer
    /// force profile in `PhysicsConfig::for_node_count`.
    pub fn for_node_count(node_count: usize) -> Self {
        let base = Self::default();
        if node_count <= super::super::physics::config::LARGE_GRAPH_THRESHOLD {
            return base;
        }
        Self {
            rest_length_multiplier: 0.9, // 1.2 → 0.9: shorter edges, tighter map
            ..base
        }
    }

    /// The y-target (model px) for a node at dependency depth `depth`.
    pub fn y_target(&self, depth: u32) -> f32 {
        let d = depth as f32 * self.level_spacing;
        if self.y_axis_down {
            d
        } else {
            -d
        }
    }

    /// The spring rest length: `level_spacing · rest_length_multiplier`.
    pub fn rest_length(&self) -> f32 {
        self.level_spacing * self.rest_length_multiplier
    }
}

/// Knobs for the [`super::hierarchical::Hierarchical`] backend — a module-tree
/// layout (Plan v2 §7.1: "force-directed within services/modules", deterministic
/// tree at the module level). `Copy` so the backend holds its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HierarchicalConfig {
    /// Vertical distance (model px) between successive levels of the module
    /// tree (a deeper module sits this far below its parent).
    pub module_v_spacing: f32,
    /// Horizontal distance (model px) between adjacent module subtrees — the
    /// leaf-to-leaf stride of the tidy tree pass.
    pub module_h_spacing: f32,
    /// Ring spacing (model px) for the compact circle that packs a module's own
    /// nodes around its tree position.
    pub intra_spacing: f32,
}

impl Default for HierarchicalConfig {
    fn default() -> Self {
        Self {
            module_v_spacing: 240.0,
            module_h_spacing: 220.0,
            intra_spacing: 30.0,
        }
    }
}

/// Knobs for the [`super::stable_grid::StableGrid`] backend — a top-level
/// service grid with deterministic, memorisable cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StableGridConfig {
    /// Grid width (cells per row). Services flow row-major into the grid.
    pub grid_cols: usize,
    /// Distance (model px) between adjacent service-cell centres, both axes.
    pub cell_size: f32,
    /// Ring spacing (model px) for the compact circle that packs a service's
    /// nodes around its cell centre.
    pub intra_spacing: f32,
}

impl Default for StableGridConfig {
    fn default() -> Self {
        Self {
            grid_cols: 4,
            cell_size: 380.0,
            intra_spacing: 30.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan_v2_7_5() {
        let c = LayoutConfig::default();
        assert_eq!(c.level_spacing, 120.0);
        assert_eq!(c.rest_length_multiplier, 1.2);
        assert_eq!(c.rest_length(), 144.0);
        assert!(c.y_axis_down);
    }

    #[test]
    fn large_graphs_get_shorter_edges() {
        use super::super::super::physics::config::LARGE_GRAPH_THRESHOLD;
        // Small graphs keep the locked geometry.
        assert_eq!(LayoutConfig::for_node_count(0), LayoutConfig::default());
        assert_eq!(
            LayoutConfig::for_node_count(LARGE_GRAPH_THRESHOLD),
            LayoutConfig::default()
        );
        // Large graphs shorten the rest length for a tighter map.
        let big = LayoutConfig::for_node_count(LARGE_GRAPH_THRESHOLD + 1);
        assert!(big.rest_length_multiplier < LayoutConfig::default().rest_length_multiplier);
        assert!(big.rest_length() < LayoutConfig::default().rest_length());
    }

    #[test]
    fn y_target_grows_downward_by_default() {
        let c = LayoutConfig::default();
        assert_eq!(c.y_target(0), 0.0);
        assert_eq!(c.y_target(2), 240.0); // deeper = larger y = lower on screen
    }

    #[test]
    fn y_axis_can_flip() {
        let c = LayoutConfig {
            y_axis_down: false,
            ..Default::default()
        };
        assert_eq!(c.y_target(2), -240.0);
    }

    #[test]
    fn deterministic_backend_configs_have_sane_defaults() {
        let h = HierarchicalConfig::default();
        assert!(h.module_v_spacing > 0.0 && h.module_h_spacing > 0.0);
        assert!(h.intra_spacing > 0.0);

        let g = StableGridConfig::default();
        assert_eq!(g.grid_cols, 4);
        assert!(g.cell_size > 0.0 && g.intra_spacing > 0.0);
    }
}
