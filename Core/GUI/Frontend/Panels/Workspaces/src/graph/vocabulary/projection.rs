//! PURE anchors → graph projection (Slice N, Plan v2 §4/§6: the vocabulary
//! world-model drawn over the code graph).
//!
//! Two steps, split so the per-frame cost stays trivial:
//!
//!   1. [`resolve`] — once per anchors/graph load: match each anchor's
//!      target symbol to a code-graph node id (anchors whose target doesn't
//!      resolve are treated as concepts).
//!   2. [`project`] — per frame: turn the resolved set + the CURRENT layout
//!      into nodes ([`NodeKind::Anchor`] → theme key `anchor_concept`),
//!      edges ([`RelType::RelatedTo`] → theme key `related_to`), and
//!      positions. Code-target anchors orbit their symbol's position;
//!      concepts spiral at the graph's edge.
//!
//! Geometry constants here are *layout* constants (model-space placement,
//! like the scaffold spiral's spacing) — every *visual* value (colour, size,
//! icon overlay, edge style) comes from the Theme via the pre-provisioned
//! `anchor_concept` / `related_to` keys.

use std::collections::{HashMap, HashSet};

use crate::graph::model::{Edge, Layout, Node, NodeKind, Position, RelType, WorkspaceGraph};

/// Synthetic-node id namespace, mirroring C-cluster's `cluster::` prefix.
pub const ANCHOR_NODE_PREFIX: &str = "anchor::";

/// Model-space orbit radius for a code-target anchor around its symbol.
const ORBIT_RADIUS: f32 = 42.0;
/// Gap between the code graph's outer radius and the concept spiral.
const CONCEPT_EDGE_MARGIN: f32 = 90.0;
/// Spacing of successive concepts along the edge spiral.
const CONCEPT_SPIRAL_SPACING: f32 = 55.0;
/// Golden angle (radians) — same constant family as the scaffold spiral.
const GOLDEN_ANGLE: f32 = 2.399_963;

/// What the projection needs to know about one anchor. The GraphView builds
/// these from the Vocabulary tab's wire mirror (`vocabulary::ipc::AnchorView`)
/// so this module stays IPC-free.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnchorSpec {
    pub identifier: String,
    /// The code symbol it targets, if any (`None` → concept anchor).
    pub target_symbol: Option<String>,
    /// Peer anchors (bare identifiers) — OI-22 connections.
    pub related_to: Vec<String>,
}

/// An anchor with its symbol target resolved against a loaded graph.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAnchor {
    pub identifier: String,
    /// The code-graph node the anchor orbits; `None` → concept (or a stale
    /// target — both spiral at the edge).
    pub symbol_node_id: Option<String>,
    pub related_to: Vec<String>,
}

/// The per-frame projection output, appended by `overlay::apply`.
#[derive(Clone, Debug, Default)]
pub struct VocabularyProjection {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub positions: HashMap<String, Position>,
}

/// The synthetic node id for an anchor identifier.
pub fn anchor_node_id(identifier: &str) -> String {
    format!("{ANCHOR_NODE_PREFIX}{identifier}")
}

/// Resolve each anchor's target symbol to a graph node (by node name, then
/// node id — the symbol store keys on names; ids disambiguate). Run once per
/// load, not per frame.
pub fn resolve(anchors: &[AnchorSpec], graph: &WorkspaceGraph) -> Vec<ResolvedAnchor> {
    anchors
        .iter()
        .map(|a| {
            let symbol_node_id = a.target_symbol.as_deref().and_then(|sym| {
                graph
                    .nodes
                    .iter()
                    .find(|n| n.name == sym)
                    .or_else(|| graph.nodes.iter().find(|n| n.id == sym))
                    .map(|n| n.id.clone())
            });
            ResolvedAnchor {
                identifier: a.identifier.clone(),
                symbol_node_id,
                related_to: a.related_to.clone(),
            }
        })
        .collect()
}

/// Project the resolved anchors against the CURRENT layout. Pure and cheap —
/// O(anchors) layout lookups, so it can run every physics frame.
pub fn project(anchors: &[ResolvedAnchor], layout: &Layout) -> VocabularyProjection {
    let mut out = VocabularyProjection::default();
    if anchors.is_empty() {
        return out;
    }

    // The code graph's outer radius — the concept spiral starts past it.
    let graph_radius = layout
        .iter()
        .map(|(_, p)| (p.x * p.x + p.y * p.y).sqrt())
        .fold(0.0_f32, f32::max);

    // Stable iteration order → stable placement (anchors arrive sorted from
    // the stores, but don't depend on it).
    let mut sorted: Vec<&ResolvedAnchor> = anchors.iter().collect();
    sorted.sort_unstable_by(|a, b| a.identifier.cmp(&b.identifier));

    // How many anchors share each symbol, to spread their orbit angles.
    let mut orbit_index: HashMap<&str, usize> = HashMap::new();
    let mut concept_index = 0usize;

    for a in &sorted {
        let pos = match a
            .symbol_node_id
            .as_deref()
            .and_then(|id| layout.get(id).map(|p| (id, p)))
        {
            Some((sym_id, centre)) => {
                let k = orbit_index.entry(sym_id).or_insert(0);
                let angle = *k as f32 * GOLDEN_ANGLE;
                *k += 1;
                Position {
                    x: centre.x + ORBIT_RADIUS * angle.cos(),
                    y: centre.y + ORBIT_RADIUS * angle.sin(),
                    z: 0.0,
                }
            }
            None => {
                // Concept (or unresolved target): golden-angle spiral past
                // the graph's edge.
                let idx = concept_index as f32;
                concept_index += 1;
                let angle = idx * GOLDEN_ANGLE;
                let radius =
                    graph_radius + CONCEPT_EDGE_MARGIN + CONCEPT_SPIRAL_SPACING * idx.sqrt();
                Position {
                    x: radius * angle.cos(),
                    y: radius * angle.sin(),
                    z: 0.0,
                }
            }
        };
        let id = anchor_node_id(&a.identifier);
        out.positions.insert(id.clone(), pos);
        out.nodes.push(Node {
            id,
            kind: NodeKind::Anchor,
            name: a.identifier.clone(),
            file: String::new(),
            line: 0,
            position: pos,
            style: Default::default(),
        });
    }

    // Edges: anchor → its symbol (the tether), and anchor → anchor for each
    // OI-22 connection whose peer is in the set. Peer edges dedup by
    // unordered pair (both directions may be stored).
    let in_set: HashSet<&str> = sorted.iter().map(|a| a.identifier.as_str()).collect();
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    for a in &sorted {
        let src = anchor_node_id(&a.identifier);
        if let Some(sym) = &a.symbol_node_id {
            out.edges.push(Edge {
                src: src.clone(),
                dst: sym.clone(),
                rel_type: RelType::RelatedTo,
                weight: 1.0,
            });
        }
        for peer in &a.related_to {
            if !in_set.contains(peer.as_str()) {
                continue; // dangling connection — nothing to draw to
            }
            let dst = anchor_node_id(peer);
            let key = if src < dst {
                (src.clone(), dst.clone())
            } else {
                (dst.clone(), src.clone())
            };
            if !seen_pairs.insert(key) {
                continue;
            }
            out.edges.push(Edge {
                src: src.clone(),
                dst,
                rel_type: RelType::RelatedTo,
                weight: 1.0,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with(names: &[&str]) -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: names
                .iter()
                .map(|n| Node {
                    id: format!("id-{n}"),
                    kind: NodeKind::Function,
                    name: (*n).to_owned(),
                    file: format!("src/{n}.rs"),
                    line: 0,
                    position: Position::default(),
                    style: Default::default(),
                })
                .collect(),
            edges: vec![],
            clusters: vec![],
        }
    }

    fn spec(id: &str, target: Option<&str>, related: &[&str]) -> AnchorSpec {
        AnchorSpec {
            identifier: id.to_owned(),
            target_symbol: target.map(str::to_owned),
            related_to: related.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn resolve_matches_name_then_id_else_concept() {
        let g = graph_with(&["alpha", "beta"]);
        let resolved = resolve(
            &[
                spec("a1", Some("alpha"), &[]),
                spec("a2", Some("id-beta"), &[]),
                spec("c1", None, &[]),
                spec("gone", Some("vanished_symbol"), &[]),
            ],
            &g,
        );
        assert_eq!(resolved[0].symbol_node_id.as_deref(), Some("id-alpha"));
        assert_eq!(resolved[1].symbol_node_id.as_deref(), Some("id-beta"));
        assert_eq!(resolved[2].symbol_node_id, None);
        assert_eq!(resolved[3].symbol_node_id, None, "stale target → concept");
    }

    #[test]
    fn code_target_anchors_orbit_their_symbol() {
        let g = graph_with(&["alpha"]);
        let layout = g.scaffold_layout();
        let resolved = resolve(
            &[
                spec("a1", Some("alpha"), &[]),
                spec("a2", Some("alpha"), &[]),
            ],
            &g,
        );
        let proj = project(&resolved, &layout);
        let centre = layout.get("id-alpha").unwrap();
        for ident in ["a1", "a2"] {
            let p = proj.positions[&anchor_node_id(ident)];
            let d = ((p.x - centre.x).powi(2) + (p.y - centre.y).powi(2)).sqrt();
            assert!(
                (d - ORBIT_RADIUS).abs() < 1e-3,
                "{ident} sits on the orbit ring (d = {d})"
            );
        }
        // Two anchors on one symbol spread to distinct angles.
        let p1 = proj.positions[&anchor_node_id("a1")];
        let p2 = proj.positions[&anchor_node_id("a2")];
        assert!((p1.x - p2.x).abs() > 1.0 || (p1.y - p2.y).abs() > 1.0);
        // Each carries the symbol tether edge.
        assert_eq!(
            proj.edges
                .iter()
                .filter(|e| e.dst == "id-alpha" && e.rel_type == RelType::RelatedTo)
                .count(),
            2
        );
    }

    #[test]
    fn concepts_spiral_past_the_graph_edge() {
        let g = graph_with(&["alpha", "beta", "gamma"]);
        let layout = g.scaffold_layout();
        let graph_radius = layout
            .iter()
            .map(|(_, p)| (p.x * p.x + p.y * p.y).sqrt())
            .fold(0.0_f32, f32::max);
        let resolved = resolve(&[spec("idea", None, &[]), spec("notion", None, &[])], &g);
        let proj = project(&resolved, &layout);
        for ident in ["idea", "notion"] {
            let p = proj.positions[&anchor_node_id(ident)];
            let r = (p.x * p.x + p.y * p.y).sqrt();
            assert!(
                r >= graph_radius + CONCEPT_EDGE_MARGIN - 1e-3,
                "{ident} lies outside the code graph (r = {r}, graph = {graph_radius})"
            );
        }
    }

    #[test]
    fn peer_edges_dedup_and_skip_dangling() {
        let g = graph_with(&[]);
        let layout = Layout::default();
        // a↔b stored in both directions + a → missing peer.
        let resolved = resolve(
            &[
                spec("a", None, &["b", "not_an_anchor"]),
                spec("b", None, &["a"]),
            ],
            &g,
        );
        let proj = project(&resolved, &layout);
        let peer_edges: Vec<_> = proj
            .edges
            .iter()
            .filter(|e| {
                e.src.starts_with(ANCHOR_NODE_PREFIX) && e.dst.starts_with(ANCHOR_NODE_PREFIX)
            })
            .collect();
        assert_eq!(peer_edges.len(), 1, "unordered pair drawn once");
        assert!(proj.edges.iter().all(|e| e.rel_type == RelType::RelatedTo));
    }

    #[test]
    fn anchor_nodes_use_the_anchor_kind_and_namespace() {
        let g = graph_with(&["alpha"]);
        let layout = g.scaffold_layout();
        let proj = project(&resolve(&[spec("a1", Some("alpha"), &[])], &g), &layout);
        assert_eq!(proj.nodes.len(), 1);
        assert_eq!(proj.nodes[0].kind, NodeKind::Anchor);
        assert_eq!(proj.nodes[0].kind.theme_key(), "anchor_concept");
        assert!(proj.nodes[0].id.starts_with(ANCHOR_NODE_PREFIX));
        assert!(project(&[], &layout).nodes.is_empty());
    }
}
