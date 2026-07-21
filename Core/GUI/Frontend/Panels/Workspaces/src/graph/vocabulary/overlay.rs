//! The vocabulary display-graph transform (Slice N) — C-cluster's
//! render-only precedent applied to the anchor layer.
//!
//! [`apply`] derives the graph + layout the renderer draws for the active
//! [`ViewMode`]; the REAL graph, layout, and physics worker are untouched
//! (anchors don't participate in the simulation — they're projected onto it
//! every frame).
//!
//!   * `CodeGraph`        → `None` (draw the base, no anchor cost).
//!   * `Overlay`          → base + the projection appended.
//!   * `VocabularyGraph`  → the anchor world-model alone (peer connections
//!     only; symbol tethers point at nodes that aren't drawn in this mode).

use std::collections::HashMap;

use super::projection::{VocabularyProjection, ANCHOR_NODE_PREFIX};
use crate::graph::model::{Layout, Position, ViewMode, WorkspaceGraph};

/// Derive the display graph + layout for `mode`. `None` → draw the base
/// (also when there is nothing to project — an empty vocabulary never costs
/// a graph rebuild).
pub fn apply(
    mode: ViewMode,
    base_graph: &WorkspaceGraph,
    base_layout: &Layout,
    proj: &VocabularyProjection,
) -> Option<(WorkspaceGraph, Layout)> {
    if mode == ViewMode::CodeGraph || proj.nodes.is_empty() {
        return None;
    }
    match mode {
        ViewMode::Overlay => {
            let mut graph = base_graph.clone();
            graph.nodes.extend(proj.nodes.iter().cloned());
            graph.edges.extend(proj.edges.iter().cloned());
            let mut positions: HashMap<String, Position> =
                base_layout.iter().map(|(id, p)| (id.clone(), *p)).collect();
            positions.extend(proj.positions.clone());
            Some((graph, Layout::from_positions(positions)))
        }
        ViewMode::VocabularyGraph => {
            let graph = WorkspaceGraph {
                nodes: proj.nodes.clone(),
                edges: proj
                    .edges
                    .iter()
                    .filter(|e| {
                        e.src.starts_with(ANCHOR_NODE_PREFIX)
                            && e.dst.starts_with(ANCHOR_NODE_PREFIX)
                    })
                    .cloned()
                    .collect(),
                clusters: vec![],
            };
            Some((graph, Layout::from_positions(proj.positions.clone())))
        }
        ViewMode::CodeGraph => unreachable!("early-returned above"),  // INVARIANT: CodeGraph early-returns above this match arm. wylde-check: panel-panic-allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Edge, Node, NodeKind, RelType};
    use crate::graph::vocabulary::projection::{project, resolve, AnchorSpec};

    fn base() -> (WorkspaceGraph, Layout) {
        let g = WorkspaceGraph {
            nodes: vec![Node {
                id: "id-alpha".to_owned(),
                kind: NodeKind::Function,
                name: "alpha".to_owned(),
                file: "src/alpha.rs".to_owned(),
                line: 0,
                position: Position::default(),
                style: Default::default(),
            }],
            edges: vec![],
            clusters: vec![],
        };
        let l = g.scaffold_layout();
        (g, l)
    }

    fn proj_for(base_g: &WorkspaceGraph, base_l: &Layout) -> VocabularyProjection {
        let specs = vec![
            AnchorSpec {
                identifier: "a1".to_owned(),
                target_symbol: Some("alpha".to_owned()),
                related_to: vec!["idea".to_owned()],
            },
            AnchorSpec {
                identifier: "idea".to_owned(),
                target_symbol: None,
                related_to: vec![],
            },
        ];
        project(&resolve(&specs, base_g), base_l)
    }

    #[test]
    fn code_graph_mode_and_empty_projection_draw_the_base() {
        let (g, l) = base();
        let proj = proj_for(&g, &l);
        assert!(apply(ViewMode::CodeGraph, &g, &l, &proj).is_none());
        assert!(apply(ViewMode::Overlay, &g, &l, &VocabularyProjection::default()).is_none());
    }

    #[test]
    fn overlay_appends_without_touching_the_base() {
        let (g, l) = base();
        let proj = proj_for(&g, &l);
        let (dg, dl) = apply(ViewMode::Overlay, &g, &l, &proj).expect("overlay");
        assert_eq!(dg.nodes.len(), 1 + 2, "base node + two anchors");
        // Tether (a1 → id-alpha) + peer (a1 ↔ idea).
        assert_eq!(dg.edges.len(), 2);
        assert!(dg.edges.iter().all(|e| e.rel_type == RelType::RelatedTo));
        // Every drawn node has a position; the base layout object is intact.
        assert!(dg.nodes.iter().all(|n| dl.get(&n.id).is_some()));
        assert_eq!(l.len(), 1, "base layout untouched");
        assert_eq!(g.nodes.len(), 1, "base graph untouched");
    }

    #[test]
    fn vocabulary_mode_draws_anchors_only_and_drops_tethers() {
        let (g, l) = base();
        let proj = proj_for(&g, &l);
        let (dg, dl) = apply(ViewMode::VocabularyGraph, &g, &l, &proj).expect("vocab");
        assert_eq!(dg.nodes.len(), 2);
        assert!(dg
            .nodes
            .iter()
            .all(|n| n.id.starts_with(ANCHOR_NODE_PREFIX)));
        assert_eq!(dg.edges.len(), 1, "peer edge only — the tether is dropped");
        let e: &Edge = &dg.edges[0];
        assert!(e.src.starts_with(ANCHOR_NODE_PREFIX) && e.dst.starts_with(ANCHOR_NODE_PREFIX));
        assert_eq!(dl.len(), 2, "anchor positions only");
    }
}
