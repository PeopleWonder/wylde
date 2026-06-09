//! WHERE things go — the pluggable layout layer (Build Order §4
//! `graph/layout/`). A *layout backend* turns a [`WorkspaceGraph`] into node
//! positions. C-physics installed the first backend,
//! [`force_directed::ForceDirected`]; C-layout adds two deterministic backends
//! ([`hierarchical::Hierarchical`], [`stable_grid::StableGrid`]) and an animated
//! 500 ms transition between them ([`transition`]).
//!
//! ## Naming: `LayoutBackend`, not `Layout`
//!
//! Build Order §4 labels this trait "Layout trait", but C-scaffold already
//! bound the name [`crate::graph::model::Layout`] to the **positions
//! container** (an id → position map) — the *output* a backend produces.
//! Introducing a second `Layout` (the trait) that the same modules import
//! would be a footgun, so the trait is [`LayoutBackend`] and a backend returns
//! a `model::Layout`. This is a collision-avoidance rename, not a behavioural
//! divergence from the spec.

pub mod config;
pub mod force_directed;
pub mod hierarchical;
pub mod pack;
pub mod stable_grid;
pub mod transition;

pub use config::{HierarchicalConfig, LayoutConfig, StableGridConfig};
pub use force_directed::ForceDirected;
pub use hierarchical::Hierarchical;
pub use stable_grid::StableGrid;
pub use transition::{CubicBezier, LayoutTransition};

use crate::graph::model::{Layout, WorkspaceGraph};

/// A pluggable layout strategy: graph → positions. Force-directed is iterative
/// (the returned `Layout` is a warm-start the physics worker then refines);
/// the deterministic backends ([`Hierarchical`], [`StableGrid`]) return their
/// final positions straight from [`compute_positions`](LayoutBackend::compute_positions).
pub trait LayoutBackend {
    /// Stable identifier for the backend (matches [`LayoutKind::name`]; used to
    /// remember the chosen layout per workspace).
    fn name(&self) -> &'static str;

    /// Compute positions for every node in `graph`. For force-directed this is
    /// the warm-start seed; for the deterministic backends it is the final
    /// layout.
    fn compute_positions(&self, graph: &WorkspaceGraph) -> Layout;
}

/// Which layout a [`crate::graph::GraphView`] is showing. Selectable per
/// workspace; cycled by `Ctrl+Shift+L`. [`ForceDirected`](LayoutKind::ForceDirected)
/// is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutKind {
    /// Physics-driven force layout (C-physics). The off-thread worker keeps
    /// refining; the only non-deterministic, animated-by-physics layout.
    #[default]
    ForceDirected,
    /// Module-grouped deterministic tree (C-layout). Physics paused.
    Hierarchical,
    /// Top-level service grid (C-layout). Physics paused.
    StableGrid,
}

impl LayoutKind {
    /// The backend's stable name (matches [`LayoutBackend::name`]). Used for
    /// per-workspace persistence.
    pub fn name(self) -> &'static str {
        match self {
            LayoutKind::ForceDirected => "force_directed",
            LayoutKind::Hierarchical => "hierarchical",
            LayoutKind::StableGrid => "stable_grid",
        }
    }

    /// Parse a persisted [`name`](Self::name) back to a kind. Unknown → `None`
    /// (caller falls back to the default).
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "force_directed" => Some(LayoutKind::ForceDirected),
            "hierarchical" => Some(LayoutKind::Hierarchical),
            "stable_grid" => Some(LayoutKind::StableGrid),
            _ => None,
        }
    }

    /// The next layout in the cycle `force → hierarchical → grid → force`
    /// (the `Ctrl+Shift+L` order).
    pub fn next(self) -> Self {
        match self {
            LayoutKind::ForceDirected => LayoutKind::Hierarchical,
            LayoutKind::Hierarchical => LayoutKind::StableGrid,
            LayoutKind::StableGrid => LayoutKind::ForceDirected,
        }
    }

    /// Whether this layout is driven by the physics worker (force-directed) vs.
    /// static/deterministic (hierarchical, stable-grid). Deterministic layouts
    /// keep the worker paused.
    pub fn is_physics(self) -> bool {
        matches!(self, LayoutKind::ForceDirected)
    }

    /// Compute this layout's positions for `graph`, dispatching to the matching
    /// backend at its default configuration. (C-settings later threads through
    /// the per-profile configs.)
    pub fn compute_positions(self, graph: &WorkspaceGraph) -> Layout {
        match self {
            LayoutKind::ForceDirected => ForceDirected::default().compute_positions(graph),
            LayoutKind::Hierarchical => Hierarchical::default().compute_positions(graph),
            LayoutKind::StableGrid => StableGrid::default().compute_positions(graph),
        }
    }

    /// Human-facing short label for the status overlay.
    pub fn label(self) -> &'static str {
        match self {
            LayoutKind::ForceDirected => "Force",
            LayoutKind::Hierarchical => "Hierarchical",
            LayoutKind::StableGrid => "Grid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_is_force_hierarchical_grid_force() {
        let mut k = LayoutKind::default();
        assert_eq!(k, LayoutKind::ForceDirected);
        k = k.next();
        assert_eq!(k, LayoutKind::Hierarchical);
        k = k.next();
        assert_eq!(k, LayoutKind::StableGrid);
        k = k.next();
        assert_eq!(k, LayoutKind::ForceDirected);
    }

    #[test]
    fn name_round_trips() {
        for k in [
            LayoutKind::ForceDirected,
            LayoutKind::Hierarchical,
            LayoutKind::StableGrid,
        ] {
            assert_eq!(LayoutKind::from_name(k.name()), Some(k));
            // The kind's name matches the backend's name.
        }
        assert_eq!(LayoutKind::from_name("bogus"), None);
    }

    #[test]
    fn kind_name_matches_backend_name() {
        assert_eq!(
            LayoutKind::ForceDirected.name(),
            ForceDirected::default().name()
        );
        assert_eq!(
            LayoutKind::Hierarchical.name(),
            Hierarchical::default().name()
        );
        assert_eq!(LayoutKind::StableGrid.name(), StableGrid::default().name());
    }

    #[test]
    fn only_force_directed_uses_physics() {
        assert!(LayoutKind::ForceDirected.is_physics());
        assert!(!LayoutKind::Hierarchical.is_physics());
        assert!(!LayoutKind::StableGrid.is_physics());
    }

    #[test]
    fn compute_positions_is_total_for_every_kind() {
        use crate::graph::model::{Node, NodeKind, Position};
        let nodes: Vec<Node> = ["a", "b", "c"]
            .iter()
            .map(|id| Node {
                id: (*id).to_owned(),
                kind: NodeKind::Function,
                name: (*id).to_owned(),
                file: format!("svc/src/{id}.rs"),
                line: 0,
                position: Position::default(),
                style: Default::default(),
            })
            .collect();
        let graph = WorkspaceGraph {
            nodes,
            edges: vec![],
            clusters: vec![],
        };
        for k in [
            LayoutKind::ForceDirected,
            LayoutKind::Hierarchical,
            LayoutKind::StableGrid,
        ] {
            let layout = k.compute_positions(&graph);
            assert_eq!(layout.len(), 3, "{} places every node", k.name());
            for id in ["a", "b", "c"] {
                assert!(layout.get(id).is_some(), "{} placed {id}", k.name());
            }
        }
    }
}
