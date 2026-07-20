//! Pure model for the concept-routing **R3b** typed dependency-tree view
//! (concept-routing plan §5, relation-model addendum §4.3) — the testable,
//! gpui-free logic the [`super::tree_view::DependencyTreeView`] renders.
//!
//! ## What it does
//!
//! Project the workspace's typed relation DAG (the `reducer::overview` shape —
//! the same per-node grouped edge set the Relations editor lists) into the
//! **shipped graph render stack** and draw it as a tree:
//!
//! * **Depends-on is the hierarchy.** A `Dependency` edge `P → X` (P depends-on
//!   X) places X *below* P. We synthesise each node a file-path proxy encoding
//!   its dependency-ancestor chain and run the **shipped `Hierarchical` layout
//!   backend** (`graph::layout::LayoutKind::Hierarchical`) over it, so the
//!   tidy-tree placement (parents centred above children, no overlap) is reused
//!   verbatim — no new layout engine. The edges draw with a directional
//!   arrowhead (depends-on ↓ / depended-on-by ↑).
//! * **Exclusion (IS NOT) is a severed cut.** A `Negative` edge draws as a
//!   **dashed red** cross-link — visually a break, not a link (the addendum's
//!   emphasis: tell the eye what *not* to conflate).
//! * **Positive is a light link** — a thin connector.
//!
//! Edge colours match the Relations editor's `group_color` (dependency =
//! `BRAND_LIGHT`, exclusion = `DANGER`, positive = `ACCENT_CYAN`) so the two
//! surfaces read consistently.
//!
//! ## Reuse, not reinvention
//!
//! Everything that draws is the shipped `graph::render` draw list
//! ([`EdgeDraw`]/[`SphereDraw`]/[`RenderOutput`]) painted by
//! `graph::paint::paint_graph`, projected through the shipped [`Viewport`], and
//! laid out by the shipped `Hierarchical` backend. This module only **maps the
//! relation DAG onto those primitives** + styles the three edge kinds. It is
//! read-only: nothing here mutates the relation store.

use std::collections::HashMap;

use gpui::Rgba;
use wylde_theme::colors::{ACCENT_CYAN, BRAND, BRAND_DIM, BRAND_LIGHT, DANGER, TEXT_MUTED};

use crate::graph::layout::LayoutKind;
use crate::graph::model::{Edge, Layout, Node, NodeKind, Position, RelType, WorkspaceGraph};
use crate::graph::render::{Color, EdgeDraw, RenderOutput, SphereDraw, SphereLayer, Viewport};

use super::ipc::{NodeItem, NodeRefView, RelationKindView, RelationView};
use super::reducer::{self, OverviewRow};

/// `gpui::Rgba` → the render layer's `Color` (both are 0..=1 RGBA).
fn to_color(c: Rgba) -> Color {
    Color::rgba(c.r, c.g, c.b, c.a)
}

/// The visual treatment of one relation kind in the tree (concept-routing plan
/// §5 / addendum §4.3). Pure data so the mapping is unit-testable without a
/// window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeKindStyle {
    pub color: Color,
    /// Drawn as a dashed (severed) line — the exclusion "cut".
    pub dashed: bool,
    /// Drawn with an arrowhead toward `to` — the hierarchy direction.
    pub directional: bool,
    pub thickness: f32,
    /// The human label for the kind ("depends-on", "relates-to", "IS NOT").
    pub label: &'static str,
}

/// Map a relation kind to its tree edge treatment. The colours mirror the
/// Relations editor's `group_color` so the two surfaces agree.
pub fn edge_kind_style(kind: RelationKindView) -> EdgeKindStyle {
    match kind {
        // Dependency = the hierarchy backbone: solid, directional, bright.
        RelationKindView::Dependency => EdgeKindStyle {
            color: to_color(BRAND_LIGHT),
            dashed: false,
            directional: true,
            thickness: 2.0,
            label: "depends-on",
        },
        // Positive = a lighter, undirected link.
        RelationKindView::Positive => EdgeKindStyle {
            color: to_color(ACCENT_CYAN).with_alpha(0.7),
            dashed: false,
            directional: false,
            thickness: 1.2,
            label: "relates-to",
        },
        // Negative = a SEVERED, dashed red cut — never reads as a link.
        RelationKindView::Negative => EdgeKindStyle {
            color: to_color(DANGER),
            dashed: true,
            directional: false,
            thickness: 1.6,
            label: "IS NOT",
        },
    }
}

/// One node in the projected tree: its stable synthetic id (`token`, used as the
/// `WorkspaceGraph` node id + render/hit-test key), its relation-graph identity,
/// and its display label.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub token: String,
    pub node: NodeRefView,
    pub label: String,
}

/// One typed edge to draw, in terms of node tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeEdge {
    pub from: String,
    pub to: String,
    pub kind: RelationKindView,
}

/// The projected tree: the node set, the typed edges, the synthesised
/// `WorkspaceGraph` the shipped layout backend consumes, and the token →
/// `NodeRefView` map (for hit-test → deep-link).
#[derive(Clone, Debug, Default)]
pub struct TreeModel {
    pub nodes: Vec<TreeNode>,
    pub edges: Vec<TreeEdge>,
    pub graph: WorkspaceGraph,
    token_of: HashMap<NodeRefView, String>,
    node_of: HashMap<String, NodeRefView>,
}

impl TreeModel {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The relation-graph node a render token maps back to (hit-test → deep-link).
    pub fn node_for_token(&self, token: &str) -> Option<&NodeRefView> {
        self.node_of.get(token)
    }

    /// The render/layout token for a relation-graph node (the inverse of
    /// [`node_for_token`](Self::node_for_token)).
    pub fn token_for(&self, node: &NodeRefView) -> Option<&str> {
        self.token_of.get(node).map(String::as_str)
    }

    /// Compute the tidy-tree positions via the **shipped `Hierarchical` layout
    /// backend** (depends-on becomes the hierarchy via the synthesised file
    /// paths).
    pub fn layout(&self) -> Layout {
        LayoutKind::Hierarchical.compute_positions(&self.graph)
    }
}

/// Build the tree model from the relation overview (the `reducer::overview`
/// shape — `reducer::overview(&relations)`) and the node universe (for labels).
///
/// `Dependency` edges define the hierarchy; `Positive`/`Negative` overlay as
/// cross-links. A node reached only as a dependency *target* still appears
/// (collected from edge endpoints). Deterministic: nodes are tokenised in
/// first-seen order, the canonical parent of a multi-parent (DAG) node is the
/// lexicographically-smallest depender, and cycles fall back to roots.
pub fn build_tree(rows: &[OverviewRow], universe: &[NodeItem]) -> TreeModel {
    // 1. Flatten the overview back to its edge set (each edge appears once,
    //    bucketed under its `from`); collect distinct nodes in first-seen order.
    let mut order: Vec<NodeRefView> = Vec::new();
    let mut seen: HashMap<NodeRefView, ()> = HashMap::new();
    let mut see = |n: &NodeRefView, order: &mut Vec<NodeRefView>| {
        if seen.insert(n.clone(), ()).is_none() {
            order.push(n.clone());
        }
    };
    let mut rels: Vec<RelationView> = Vec::new();
    for row in rows {
        see(&row.node, &mut order);
        for e in &row.edges {
            see(&e.from, &mut order);
            see(&e.to, &mut order);
            rels.push(e.clone());
        }
    }
    if order.is_empty() {
        return TreeModel::default();
    }

    // 2. Tokenise (collision-free ids — concept ids contain `/`, which would
    //    corrupt the file-path proxy, so we never put the raw id in a path).
    let mut token_of: HashMap<NodeRefView, String> = HashMap::new();
    let mut node_of: HashMap<String, NodeRefView> = HashMap::new();
    for (i, n) in order.iter().enumerate() {
        let token = format!("t{i}");
        token_of.insert(n.clone(), token.clone());
        node_of.insert(token, n.clone());
    }

    // 3. Canonical-parent map for the hierarchy: a Dependency edge `from → to`
    //    means `from` depends-on `to`, so `to` is a CHILD of `from`. Pick the
    //    smallest-token depender as each child's canonical parent (other
    //    dependers' edges remain cross-links).
    let mut parents: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rels {
        if r.kind == RelationKindView::Dependency {
            let (Some(p), Some(c)) = (token_of.get(&r.from), token_of.get(&r.to)) else {
                continue;
            };
            parents.entry(c.clone()).or_default().push(p.clone());
        }
    }
    for ps in parents.values_mut() {
        ps.sort();
        ps.dedup();
    }
    let canonical_parent: HashMap<String, String> = parents
        .iter()
        .filter_map(|(c, ps)| ps.first().map(|p| (c.clone(), p.clone())))
        .collect();

    // 4. File-path proxy per node = its dependency-ancestor chain of tokens,
    //    so module-grouped Hierarchical reconstructs the depends-on tree
    //    (each node's module is unique → one node per module). Cycle-safe.
    let mut path_cache: HashMap<String, String> = HashMap::new();
    for (i, _) in order.iter().enumerate() {
        let token = format!("t{i}");
        let path = ancestor_path(&token, &canonical_parent, &mut path_cache);
        path_cache.insert(token, path);
    }

    // 5. Synthesise the WorkspaceGraph (nodes + a Dependency-only edge set is
    //    enough for the layout; the renderer draws every kind from `edges`).
    let mut nodes: Vec<TreeNode> = Vec::with_capacity(order.len());
    let mut graph_nodes: Vec<Node> = Vec::with_capacity(order.len());
    for n in &order {
        let token = token_of[n].clone();
        let label = reducer::label_for(n, universe);
        let path = path_cache
            .get(&token)
            .cloned()
            .unwrap_or_else(|| token.clone());
        graph_nodes.push(Node {
            id: token.clone(),
            kind: match n {
                NodeRefView::Concept { .. } => NodeKind::Class,
                NodeRefView::Vocab { .. } => NodeKind::Anchor,
            },
            name: label.clone(),
            // module_of(file) = the path; one node per module ⇒ the tidy tree
            // lays the dependency hierarchy out directly.
            file: format!("{path}/n.rs"),
            line: 0,
            position: Position::default(),
            style: Default::default(),
        });
        nodes.push(TreeNode {
            token,
            node: n.clone(),
            label,
        });
    }
    let graph_edges: Vec<Edge> = rels
        .iter()
        .filter(|r| r.kind == RelationKindView::Dependency)
        .filter_map(|r| {
            let (s, d) = (token_of.get(&r.from)?, token_of.get(&r.to)?);
            Some(Edge {
                src: s.clone(),
                dst: d.clone(),
                rel_type: RelType::Imports,
                weight: 1.0,
            })
        })
        .collect();

    let edges: Vec<TreeEdge> = rels
        .iter()
        .filter_map(|r| {
            Some(TreeEdge {
                from: token_of.get(&r.from)?.clone(),
                to: token_of.get(&r.to)?.clone(),
                kind: r.kind,
            })
        })
        .collect();

    TreeModel {
        nodes,
        edges,
        graph: WorkspaceGraph {
            nodes: graph_nodes,
            edges: graph_edges,
            clusters: vec![],
        },
        token_of,
        node_of,
    }
}

/// The slash-joined ancestor chain ending in `token` (e.g. `t0/t3/t7`), walked
/// up the canonical-parent map. Cycle-safe: a token already on the current
/// walk is treated as a root (the chain restarts at the cycle break), so a
/// mutual `A depends-on B; B depends-on A` never loops.
fn ancestor_path(
    token: &str,
    canonical_parent: &HashMap<String, String>,
    cache: &mut HashMap<String, String>,
) -> String {
    // Walk parents into a chain (root-first), guarding against cycles.
    let mut chain: Vec<String> = vec![token.to_owned()];
    let mut on_path: std::collections::HashSet<String> =
        std::collections::HashSet::from([token.to_owned()]);
    let mut cur = token.to_owned();
    while let Some(p) = canonical_parent.get(&cur) {
        if on_path.contains(p) {
            break; // cycle — stop; the deepest seen becomes the root
        }
        on_path.insert(p.clone());
        chain.push(p.clone());
        cur = p.clone();
    }
    chain.reverse(); // root → … → token
    let path = chain.join("/");
    cache.insert(token.to_owned(), path.clone());
    path
}

// ── Render ───────────────────────────────────────────────────────────────

/// Node radii (model px) — concepts read a touch larger than vocab terms.
const CONCEPT_RADIUS: f32 = 13.0;
const VOCAB_RADIUS: f32 = 9.0;
/// Dashed-segment geometry for the severed exclusion edge (model px).
const DASH_ON: f32 = 6.0;
const DASH_OFF: f32 = 4.0;
/// Arrowhead barb length / half-angle (radians) for directional dependency edges.
const ARROW_LEN: f32 = 9.0;
const ARROW_ANGLE: f32 = 0.5;

/// Build the draw list for the tree at `layout`, projected through `vp`. Edges
/// draw first (under the spheres); every kind is styled by [`edge_kind_style`].
/// `dark` selects the background pair. Reuses the shipped [`RenderOutput`] so
/// `graph::paint::paint_graph` draws it unchanged.
pub fn render_tree(model: &TreeModel, layout: &Layout, vp: &Viewport, dark: bool) -> RenderOutput {
    let mut edges: Vec<EdgeDraw> = Vec::new();
    let radius_of: HashMap<&str, f32> = model
        .nodes
        .iter()
        .map(|n| {
            (
                n.token.as_str(),
                if n.node.is_concept() {
                    CONCEPT_RADIUS
                } else {
                    VOCAB_RADIUS
                },
            )
        })
        .collect();

    for e in &model.edges {
        let (Some(a), Some(b)) = (layout.get(&e.from), layout.get(&e.to)) else {
            continue;
        };
        let style = edge_kind_style(e.kind);
        let (x0, y0) = vp.model_to_screen(a);
        let (x1, y1) = vp.model_to_screen(b);
        let th = (style.thickness * vp.camera.zoom).clamp(0.6, 6.0);
        if style.dashed {
            push_dashed(
                &mut edges,
                (x0, y0),
                (x1, y1),
                DASH_ON * vp.camera.zoom,
                DASH_OFF * vp.camera.zoom,
                style.color,
                th,
            );
        } else {
            edges.push(segment((x0, y0), (x1, y1), style.color, th));
        }
        if style.directional {
            // Arrowhead at the `to` end, pulled back by the target's radius so
            // it sits on the node edge, pointing toward the dependency (down).
            let r = radius_of
                .get(e.to.as_str())
                .copied()
                .unwrap_or(VOCAB_RADIUS)
                * vp.camera.zoom;
            push_arrowhead(&mut edges, (x0, y0), (x1, y1), r, style.color, th);
        }
    }

    let mut spheres: Vec<SphereDraw> = Vec::with_capacity(model.nodes.len());
    let border = to_color(TEXT_MUTED).with_alpha(0.6);
    for n in &model.nodes {
        let Some(pos) = layout.get(&n.token) else {
            continue;
        };
        let (cx, cy) = vp.model_to_screen(pos);
        let base = if n.node.is_concept() {
            to_color(BRAND)
        } else {
            to_color(BRAND_DIM)
        };
        let r = (radius_of
            .get(n.token.as_str())
            .copied()
            .unwrap_or(VOCAB_RADIUS)
            * vp.camera.zoom)
            .max(2.0);
        spheres.push(SphereDraw {
            id: n.token.clone(),
            cx,
            cy,
            radius: r,
            // Two layers: a darkened rim + the core, the same back-to-front
            // idiom the shipped renderer uses (kept simple — no specular).
            layers: vec![
                SphereLayer {
                    color: base.scale_lightness(0.65),
                    dx: 0.0,
                    dy: 0.0,
                    radius: r,
                },
                SphereLayer {
                    color: base,
                    dx: 0.0,
                    dy: 0.0,
                    radius: r * 0.82,
                },
            ],
            border_color: border,
            border_width: 1.0,
        });
    }

    // Background: a neutral dark void (the tree has no Theme dependency — it
    // borrows the panel palette, not the graph Visual Style YAML).
    let (bg_inner, bg_outer) = if dark {
        (
            Color::rgba(0.05, 0.06, 0.09, 1.0),
            Color::rgba(0.02, 0.03, 0.05, 1.0),
        )
    } else {
        (
            Color::rgba(0.96, 0.97, 0.99, 1.0),
            Color::rgba(0.90, 0.92, 0.95, 1.0),
        )
    };
    RenderOutput {
        bg_inner,
        bg_outer,
        outlines: Vec::new(),
        edges,
        spheres,
    }
}

fn segment(a: (f32, f32), b: (f32, f32), color: Color, thickness: f32) -> EdgeDraw {
    EdgeDraw {
        x0: a.0,
        y0: a.1,
        x1: b.0,
        y1: b.1,
        color,
        thickness,
    }
}

/// Walk `from → to` emitting `on`-length solid segments separated by `off`
/// gaps — the severed-exclusion dash (the same approach the shipped renderer
/// uses for dashed edges).
#[allow(clippy::too_many_arguments)]
fn push_dashed(
    out: &mut Vec<EdgeDraw>,
    from: (f32, f32),
    to: (f32, f32),
    on: f32,
    off: f32,
    color: Color,
    thickness: f32,
) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let step = (on + off).max(0.5);
    let mut t = 0.0;
    let mut count = 0;
    while t < len && count < 2000 {
        let a = (from.0 + ux * t, from.1 + uy * t);
        let end = (t + on).min(len);
        let b = (from.0 + ux * end, from.1 + uy * end);
        out.push(segment(a, b, color, thickness));
        t += step;
        count += 1;
    }
}

/// Push the two barb segments of an arrowhead at the `to` end, pulled back by
/// `target_radius` so the tip rests on the node edge.
fn push_arrowhead(
    out: &mut Vec<EdgeDraw>,
    from: (f32, f32),
    to: (f32, f32),
    target_radius: f32,
    color: Color,
    thickness: f32,
) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    // Tip on the target node's rim.
    let tip = (to.0 - ux * target_radius, to.1 - uy * target_radius);
    // Two barbs rotated ±ARROW_ANGLE from the reverse direction.
    let (ca, sa) = (ARROW_ANGLE.cos(), ARROW_ANGLE.sin());
    for sign in [1.0f32, -1.0] {
        let rx = -ux * ca + (-uy) * (sign * sa);
        let ry = -uy * ca - (-ux) * (sign * sa);
        let barb = (tip.0 + rx * ARROW_LEN, tip.1 + ry * ARROW_LEN);
        out.push(segment(tip, barb, color, thickness));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::render::Camera;
    use crate::routing::ipc::RelationKindView as K;

    fn nref_c(id: &str) -> NodeRefView {
        NodeRefView::concept(id)
    }
    fn nref_v(id: &str) -> NodeRefView {
        NodeRefView::vocab(id)
    }
    fn rel(from: NodeRefView, to: NodeRefView, kind: K) -> RelationView {
        RelationView {
            from,
            to,
            kind,
            note: None,
            created_at: 0.0,
        }
    }
    fn universe() -> Vec<NodeItem> {
        vec![
            NodeItem {
                node: nref_c("nextcloud"),
                label: "Nextcloud".into(),
            },
            NodeItem {
                node: nref_v("ddns"),
                label: "{{ddns}}".into(),
            },
            NodeItem {
                node: nref_c("wylde"),
                label: "Wylde".into(),
            },
        ]
    }
    fn vp() -> Viewport {
        Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800.0,
            height: 600.0,
            camera: Camera::default(),
            dark: true,
        }
    }

    #[test]
    fn edge_kind_styles_distinguish_the_three_kinds() {
        let dep = edge_kind_style(K::Dependency);
        let pos = edge_kind_style(K::Positive);
        let neg = edge_kind_style(K::Negative);
        // Dependency = solid, directional hierarchy line.
        assert!(dep.directional && !dep.dashed);
        // Exclusion = dashed (severed), red, non-directional.
        assert!(neg.dashed && !neg.directional);
        assert_eq!(neg.color, to_color(DANGER));
        // Positive = light, non-directional, thinner than dependency.
        assert!(!pos.dashed && !pos.directional);
        assert!(pos.thickness < dep.thickness);
        // All three colours are distinct.
        assert_ne!(dep.color, neg.color);
        assert_ne!(dep.color, pos.color);
        assert_ne!(neg.color, pos.color);
    }

    #[test]
    fn build_tree_collects_all_nodes_and_edges() {
        let rels = vec![
            rel(nref_c("nextcloud"), nref_v("ddns"), K::Dependency),
            rel(nref_c("nextcloud"), nref_c("wylde"), K::Negative),
        ];
        let rows = reducer::overview(&rels);
        let model = build_tree(&rows, &universe());
        // 3 distinct nodes (nextcloud, ddns, wylde), 2 edges.
        assert_eq!(model.nodes.len(), 3);
        assert_eq!(model.edges.len(), 2);
        // Labels resolved from the universe.
        let nc = model
            .nodes
            .iter()
            .find(|n| n.node == nref_c("nextcloud"))
            .unwrap();
        assert_eq!(nc.label, "Nextcloud");
        // Token round-trips back to the relation node (hit-test → deep-link).
        assert_eq!(model.node_for_token(&nc.token), Some(&nref_c("nextcloud")));
    }

    #[test]
    fn dependency_child_lays_out_below_its_parent() {
        // nextcloud depends-on ddns ⇒ ddns is a child ⇒ greater y (down).
        let rels = vec![rel(nref_c("nextcloud"), nref_v("ddns"), K::Dependency)];
        let rows = reducer::overview(&rels);
        let model = build_tree(&rows, &universe());
        let layout = model.layout();
        let tok = |n: &NodeRefView| model.token_for(n).unwrap().to_owned();
        let parent_y = layout.get(&tok(&nref_c("nextcloud"))).unwrap().y;
        let child_y = layout.get(&tok(&nref_v("ddns"))).unwrap().y;
        assert!(
            child_y > parent_y,
            "dependency child ({child_y}) sits below its depender ({parent_y})"
        );
    }

    #[test]
    fn dependency_cycle_does_not_loop() {
        // A ↔ B mutual dependency must still build (cycle guard → one is root).
        let rels = vec![
            rel(nref_c("nextcloud"), nref_c("wylde"), K::Dependency),
            rel(nref_c("wylde"), nref_c("nextcloud"), K::Dependency),
        ];
        let rows = reducer::overview(&rels);
        let model = build_tree(&rows, &universe());
        assert_eq!(model.nodes.len(), 2);
        // Layout terminates and places both.
        let layout = model.layout();
        assert_eq!(layout.len(), 2);
    }

    #[test]
    fn render_emits_a_sphere_per_node() {
        let rels = vec![rel(nref_c("nextcloud"), nref_v("ddns"), K::Dependency)];
        let rows = reducer::overview(&rels);
        let model = build_tree(&rows, &universe());
        let layout = model.layout();
        let out = render_tree(&model, &layout, &vp(), true);
        assert_eq!(out.spheres.len(), 2, "one sphere per node");
        assert!(
            !out.edges.is_empty(),
            "the dependency edge + arrowhead drawn"
        );
    }

    #[test]
    fn negative_edge_renders_dashed_into_multiple_segments() {
        // A long exclusion edge segments into several dashes (the severed cut).
        let rels = vec![rel(nref_c("nextcloud"), nref_c("wylde"), K::Negative)];
        let rows = reducer::overview(&rels);
        let model = build_tree(&rows, &universe());
        let layout = model.layout();
        let out = render_tree(&model, &layout, &vp(), true);
        // No arrowhead (non-directional), but multiple dash segments — strictly
        // more than the single segment a solid edge would emit, as long as the
        // nodes aren't coincident.
        let coincident = {
            let a = layout.get(&model.nodes[0].token).unwrap();
            let b = layout.get(&model.nodes[1].token).unwrap();
            (a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3
        };
        if !coincident {
            assert!(
                out.edges.len() > 1,
                "exclusion edge segments into dashes, got {}",
                out.edges.len()
            );
        }
        // Every drawn segment carries the danger colour.
        assert!(out.edges.iter().all(|e| e.color == to_color(DANGER)));
    }

    #[test]
    fn empty_overview_is_empty_model() {
        let model = build_tree(&[], &universe());
        assert!(model.is_empty());
        assert!(model.layout().is_empty());
    }

    #[test]
    fn render_output_hit_tests_back_to_a_node() {
        let rels = vec![rel(nref_c("nextcloud"), nref_v("ddns"), K::Dependency)];
        let rows = reducer::overview(&rels);
        let model = build_tree(&rows, &universe());
        let layout = model.layout();
        let out = render_tree(&model, &layout, &vp(), true);
        // A sphere's own centre hit-tests to its token, which maps to a node.
        let s = &out.spheres[0];
        let token = out.hit_test(s.cx, s.cy).unwrap();
        assert!(model.node_for_token(token).is_some());
    }
}
