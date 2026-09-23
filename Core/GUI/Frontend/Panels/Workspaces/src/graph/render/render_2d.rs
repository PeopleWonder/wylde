//! `Renderer2d` — the v1 2D implementation of the [`Renderer`] trait: real
//! radial-gradient-style spheres + per-rel-type edge lines, every value read
//! from the [`Theme`] (Visual Style v1). **No physics, no clustering, no
//! animation** — C-scaffold draws the graph statically at the layout the
//! model handed it.
//!
//! ## Faking the radial-gradient sphere
//!
//! gpui at the pinned rev has no radial gradient, so a sphere is approximated
//! by three concentric fills drawn back-to-front (Visual Style §1 sphere
//! shading):
//!
//! 1. **rim** — full radius, `base × base_color_modifier` (0.65) lightness,
//! 2. **core** — inner disc, the full-strength language colour,
//! 3. **specular** — small bright disc at `specular_intensity` alpha, offset
//!    toward `highlight_position` `(-0.25, -0.25)`.
//!
//! The lightness falloff + offset highlight read as a lit sphere against the
//! dark "space" background.
//!
//! ## Sphere size
//!
//! Uses **real graph data**: a node's size scales with its degree (incident
//! edges) through the theme's `min/max_diameter_px` and `scaling_curve`
//! (`sqrt`), then by the node-type `relative_size_multiplier` (modules ×1.4,
//! constants ×0.75). Hubs read bigger without engulfing their neighbours —
//! exactly what the `sqrt` curve is for. Degree-as-size is a reasonable
//! stand-in until C-physics introduces force-based emphasis.

use std::collections::HashMap;

use crate::graph::model::{Node, NodeKind};

use super::viewport::{rect_contains, rects_overlap};
use super::{Color, EdgeDraw, RenderOutput, Renderer, Scene, SphereDraw, SphereLayer, Viewport};

/// Radial-gradient approximation geometry — the relative radii of the core and
/// specular discs as fractions of the sphere radius. These are renderer
/// shape constants (how the fake gradient is built), not Visual Style values.
const CORE_RADIUS_FRACTION: f32 = 0.82;
const SPECULAR_RADIUS_FRACTION: f32 = 0.36;

/// Cull margin (G2): the visible model rect is grown by this fraction of its
/// half-extent before culling, so geometry just off-screen is still drawn and
/// doesn't pop in while panning.
const CULL_MARGIN_FRAC: f32 = 0.2;

/// Level-of-detail (G2): below this zoom a node is drawn as a single flat disc
/// instead of the 3-layer rim/core/specular sphere. The shading is invisible
/// at that scale and the layer count is the dominant per-sphere draw cost, so
/// dropping to one layer caps the work where the detail wouldn't read anyway.
const LOD_FLAT_DISC_ZOOM: f32 = 0.18;

/// The v1 2D renderer. Stateless today; holds no caches so a frame is a pure
/// function of `(Scene, Viewport)`. (A future renderer may cache a quadtree
/// for culling — hence `&mut self` on the trait.)
#[derive(Debug, Default)]
pub struct Renderer2d;

impl Renderer2d {
    pub fn new() -> Self {
        Renderer2d
    }
}

impl Renderer for Renderer2d {
    fn frame(&mut self, scene: &Scene<'_>, vp: &Viewport) -> RenderOutput {
        let theme = scene.theme;
        let dark = vp.dark;

        // Degree map (incident edge count) drives sphere sizing.
        let degrees = degree_map(scene);
        let max_degree = degrees.values().copied().max().unwrap_or(0);

        // Scope filter (C-navigation): when scoped into a cluster, only the
        // member nodes + fully-internal edges draw; boundary-crossing edges
        // become exit stubs appended by the view (see `Scene::scope`).
        let in_scope = |id: &str| scene.scope.is_none_or(|m| m.contains(id));

        // Viewport culling (G2): everything is tested in model space against
        // the on-screen rect (grown by a margin) so per-frame work tracks the
        // *visible* count, not the 10k/43k total.
        let view_rect = vp.visible_model_rect_expanded(CULL_MARGIN_FRAC);
        let flat_lod = vp.camera.zoom < LOD_FLAT_DISC_ZOOM;

        // ── Edges first, so spheres draw on top ─────────────────────────
        let mut edges: Vec<EdgeDraw> = Vec::new();
        for e in &scene.graph.edges {
            if !in_scope(&e.src) || !in_scope(&e.dst) {
                continue;
            }
            let (Some(a), Some(b)) = (scene.layout.get(&e.src), scene.layout.get(&e.dst)) else {
                continue; // an endpoint we have no position for — skip.
            };
            // Cull only when the *whole* segment is off-screen: an edge whose
            // endpoint-bbox doesn't touch the view rect can't be visible. A
            // segment that straddles or crosses the rect always has an
            // overlapping bbox, so a visible edge is never dropped.
            let seg_bbox = (a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y));
            if !rects_overlap(seg_bbox, view_rect) {
                continue;
            }
            let (x0, y0) = vp.model_to_screen(a);
            let (x1, y1) = vp.model_to_screen(b);

            let style = theme.edge_style(e.rel_type.theme_key());
            let color = style.map(|s| s.color(dark)).unwrap_or(Color::FALLBACK);
            let base_thickness = style.map(|s| s.thickness_px).unwrap_or(1.5);
            // Aggregate cluster→cluster edges (G1) carry the crossing count as
            // weight; scale thickness by its log so a heavily-connected galaxy
            // link reads heavier without a 1000-crossing edge swallowing the
            // canvas. A plain edge (weight 1) is unaffected (ln 1 = 0).
            let weight_factor = 1.0 + e.weight.max(1.0).ln() * 0.35;
            let thickness = (base_thickness * weight_factor * vp.camera.zoom).clamp(0.6, 6.0);
            let line_style = style.map(|s| s.line_style.as_str()).unwrap_or("solid");

            push_line(
                &mut edges,
                (x0, y0),
                (x1, y1),
                color,
                thickness,
                line_style,
                style.and_then(|s| s.dash_pattern.as_deref()),
                style.and_then(|s| s.dot_spacing_px),
                vp.camera.zoom,
            );
        }

        // ── Spheres ─────────────────────────────────────────────────────
        let highlight = theme.sphere.highlight(dark);
        let hl = theme.sphere.shading.highlight_position;
        let rim_factor = theme.sphere.shading.base_color_modifier;
        let specular_alpha = theme.sphere.shading.specular_intensity;
        let border_color = theme.sphere.border_color(dark);
        let border_width = theme.sphere.border.width_px;

        let mut spheres: Vec<SphereDraw> = Vec::with_capacity(scene.graph.nodes.len());
        for node in &scene.graph.nodes {
            if !in_scope(&node.id) {
                continue;
            }
            let Some(pos) = scene.layout.get(&node.id) else {
                continue;
            };
            // Cull off-screen spheres (G2). The margin already covers a node
            // whose centre is just outside but whose radius pokes in.
            if !rect_contains(view_rect, pos.x, pos.y) {
                continue;
            }
            let (cx, cy) = vp.model_to_screen(pos);

            let degree = degrees.get(node.id.as_str()).copied().unwrap_or(0);
            let diameter = node_diameter(theme, node.kind, degree, max_degree);
            let radius = (diameter * 0.5 * vp.camera.zoom).max(1.0);

            let base = node_base_color(theme, node, dark);

            let layers = if flat_lod {
                // LOD (G2): at very low zoom the rim falloff + offset specular
                // are sub-pixel — draw one flat disc instead of three layers.
                vec![SphereLayer {
                    color: base,
                    dx: 0.0,
                    dy: 0.0,
                    radius,
                }]
            } else {
                let rim = base.scale_lightness(rim_factor);
                vec![
                    // Rim — full disc, darkened.
                    SphereLayer {
                        color: rim,
                        dx: 0.0,
                        dy: 0.0,
                        radius,
                    },
                    // Core — full-strength colour, slightly inset.
                    SphereLayer {
                        color: base,
                        dx: 0.0,
                        dy: 0.0,
                        radius: radius * CORE_RADIUS_FRACTION,
                    },
                    // Specular — bright highlight offset toward the light.
                    SphereLayer {
                        color: highlight.with_alpha(specular_alpha),
                        dx: hl.x * radius,
                        dy: hl.y * radius,
                        radius: radius * SPECULAR_RADIUS_FRACTION,
                    },
                ]
            };

            spheres.push(SphereDraw {
                id: node.id.clone(),
                cx,
                cy,
                radius,
                layers,
                border_color,
                border_width,
            });
        }

        RenderOutput {
            bg_inner: theme.graph_panel.background.primary(dark),
            bg_outer: theme.graph_panel.background.secondary(dark),
            outlines: Vec::new(),
            edges,
            spheres,
        }
    }
}

/// The colour for a node: its language tint (by file extension), falling back
/// to a stable module-palette hue when there's no recognised language;
/// `constant` nodes are desaturated per the theme's `saturation_modifier`.
fn node_base_color(theme: &super::Theme, node: &Node, dark: bool) -> Color {
    let mut c = theme.language_color(node.language(), &node.id, dark);
    if node.kind == NodeKind::Constant {
        if let Some(keep) = theme.node_type("constant").saturation_modifier {
            c = c.desaturate(keep);
        }
    }
    c
}

/// Node diameter (px) from degree through the theme's size mapping +
/// node-type multiplier. `sqrt` curve moderates hub growth (Visual Style §1).
fn node_diameter(theme: &super::Theme, kind: NodeKind, degree: usize, max_degree: usize) -> f32 {
    let sm = &theme.sphere.size_mapping;
    let norm = if max_degree == 0 {
        0.0
    } else {
        degree as f32 / max_degree as f32
    };
    let curved = match sm.scaling_curve.as_str() {
        "sqrt" => norm.sqrt(),
        "linear" | "" => norm,
        _ => norm.sqrt(),
    };
    let base = sm.min_diameter_px + (sm.max_diameter_px - sm.min_diameter_px) * curved;
    let mult = theme.node_type(kind.theme_key()).size_multiplier();
    (base * mult).clamp(sm.min_diameter_px * 0.5, sm.max_diameter_px * 1.4)
}

/// Count incident edges per node id (both directions).
fn degree_map<'a>(scene: &Scene<'a>) -> HashMap<&'a str, usize> {
    let mut m: HashMap<&str, usize> = HashMap::new();
    for e in &scene.graph.edges {
        *m.entry(e.src.as_str()).or_default() += 1;
        *m.entry(e.dst.as_str()).or_default() += 1;
    }
    m
}

/// Append solid line segments for an edge, expanding dashed/dotted styles into
/// multiple short segments so the gpui paint layer only ever draws solids.
#[allow(clippy::too_many_arguments)]
fn push_line(
    out: &mut Vec<EdgeDraw>,
    from: (f32, f32),
    to: (f32, f32),
    color: Color,
    thickness: f32,
    line_style: &str,
    dash_pattern: Option<&[f32]>,
    dot_spacing: Option<f32>,
    zoom: f32,
) {
    match line_style {
        "dashed" => {
            let (dash, gap) = match dash_pattern {
                Some([d, g, ..]) => (*d, *g),
                _ => (6.0, 4.0),
            };
            dashed(out, from, to, dash * zoom, gap * zoom, color, thickness);
        }
        "dotted" => {
            let spacing = dot_spacing.unwrap_or(3.0) * zoom;
            // Tiny dashes approximate dots; length ≈ thickness so they read round.
            dashed(out, from, to, thickness.max(1.0), spacing, color, thickness);
        }
        _ => out.push(segment(from, to, color, thickness)),
    }
}

/// One solid segment draw command.
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
/// gaps.
#[allow(clippy::too_many_arguments)]
fn dashed(
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
    let step = (on + off).max(0.5);
    if len < f32::EPSILON {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let mut t = 0.0;
    // Guard against pathological tiny dash sizes producing thousands of segs.
    let max_segments = 2000;
    let mut count = 0;
    while t < len && count < max_segments {
        let a = (from.0 + ux * t, from.1 + uy * t);
        let end = (t + on).min(len);
        let b = (from.0 + ux * end, from.1 + uy * end);
        out.push(segment(a, b, color, thickness));
        t += step;
        count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Edge, Node, Position, RelType, ViewMode, WorkspaceGraph};
    use crate::graph::render::{Camera, Theme};

    fn node(id: &str, kind: NodeKind, file: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind,
            name: id.to_owned(),
            file: file.to_owned(),
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

    fn sample() -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![
                node("hub", NodeKind::Function, "src/a.rs"),
                node("leaf1", NodeKind::Function, "src/a.rs"),
                node("leaf2", NodeKind::Constant, "src/b.py"),
                node("Mod", NodeKind::Module, "src/c.rs"),
            ],
            edges: vec![
                edge("hub", "leaf1", RelType::Calls),
                edge("hub", "leaf2", RelType::Calls),
                edge("hub", "Mod", RelType::Imports),
            ],
            clusters: vec![],
        }
    }

    fn viewport() -> Viewport {
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
    fn frame_produces_a_sphere_per_node_and_edges() {
        let g = sample();
        let layout = g.scaffold_layout();
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        assert_eq!(out.spheres.len(), 4, "one sphere per node");
        assert!(!out.edges.is_empty(), "edges drawn");
        // Background read from theme (deep space).
        assert!(out.bg_inner.r < 0.1);
    }

    #[test]
    fn each_sphere_has_three_radial_layers() {
        let g = sample();
        let layout = g.scaffold_layout();
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        for s in &out.spheres {
            assert_eq!(s.layers.len(), 3, "rim + core + specular");
            // Specular is offset toward top-left (negative dx/dy).
            let spec = s.layers[2];
            assert!(spec.dx <= 0.0 && spec.dy <= 0.0);
            // Core radius is inset from the rim.
            assert!(s.layers[1].radius < s.layers[0].radius);
        }
    }

    #[test]
    fn hub_sphere_is_larger_than_leaf() {
        let g = sample();
        let layout = g.scaffold_layout();
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        let r = |id: &str| out.spheres.iter().find(|s| s.id == id).unwrap().radius;
        // hub has degree 3, leaf1 degree 1.
        assert!(
            r("hub") > r("leaf1"),
            "degree-3 hub bigger than degree-1 leaf"
        );
    }

    #[test]
    fn dashed_import_edge_expands_to_multiple_segments() {
        // A single IMPORTS edge (dashed) becomes many short solids.
        let g = WorkspaceGraph {
            nodes: vec![
                node("a", NodeKind::Function, "src/a.rs"),
                node("b", NodeKind::Module, "src/b.rs"),
            ],
            edges: vec![edge("a", "b", RelType::Imports)],
            clusters: vec![],
        };
        let layout = g.scaffold_layout();
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        assert!(
            out.edges.len() > 1,
            "dashed edge segments into pieces, got {}",
            out.edges.len()
        );
    }

    #[test]
    fn scoped_frame_draws_members_and_internal_edges_only() {
        // Scope = {hub, leaf1}: leaf2/Mod spheres drop; hub→leaf1 stays;
        // hub→leaf2 and hub→Mod cross the boundary so the renderer skips
        // them (they become exit stubs via navigation::compute_exit_edges).
        let g = sample();
        let layout = g.scaffold_layout();
        let theme = Theme::load_v1().unwrap();
        let members: std::collections::HashSet<String> =
            ["hub", "leaf1"].iter().map(|s| (*s).to_owned()).collect();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: Some(&members),
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        let ids: Vec<&str> = out.spheres.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"hub") && ids.contains(&"leaf1"));
        // Only the internal CALLS edge survives (solid → exactly 1 segment).
        assert_eq!(out.edges.len(), 1, "boundary edges skipped");
    }

    #[test]
    fn edge_with_unplaced_endpoint_is_skipped() {
        // Edge references a node not in the layout → no segment, no panic.
        let g = WorkspaceGraph {
            nodes: vec![node("a", NodeKind::Function, "src/a.rs")],
            edges: vec![edge("a", "ghost", RelType::Calls)],
            clusters: vec![],
        };
        let layout = g.scaffold_layout(); // only "a" placed
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        assert!(out.edges.is_empty(), "edge to unplaced node dropped");
        assert_eq!(out.spheres.len(), 1);
    }

    #[test]
    fn offscreen_node_is_culled_but_straddling_edge_survives() {
        use crate::graph::model::Layout;
        use std::collections::HashMap;
        // "near" at the origin (visible), "far" way off-screen (culled).
        let g = WorkspaceGraph {
            nodes: vec![
                node("near", NodeKind::Function, "src/a.rs"),
                node("far", NodeKind::Function, "src/b.rs"),
            ],
            edges: vec![edge("near", "far", RelType::Calls)],
            clusters: vec![],
        };
        let mut pos = HashMap::new();
        pos.insert(
            "near".to_owned(),
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        pos.insert(
            "far".to_owned(),
            Position {
                x: 100_000.0,
                y: 0.0,
                z: 0.0,
            },
        );
        let layout = Layout::from_positions(pos);
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        // Default viewport (800×600, zoom 1) sees roughly ±480/±360 model px.
        let out = Renderer2d::new().frame(&scene, &viewport());
        let ids: Vec<&str> = out.spheres.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["near"], "far sphere culled, near kept");
        // The edge straddles the viewport (one endpoint inside) — its bbox
        // overlaps the view rect, so it is NOT dropped.
        assert!(!out.edges.is_empty(), "straddling edge kept");
    }

    #[test]
    fn fully_offscreen_edge_is_culled() {
        use crate::graph::model::Layout;
        use std::collections::HashMap;
        let g = WorkspaceGraph {
            nodes: vec![
                node("a", NodeKind::Function, "src/a.rs"),
                node("b", NodeKind::Function, "src/b.rs"),
            ],
            edges: vec![edge("a", "b", RelType::Calls)],
            clusters: vec![],
        };
        let mut pos = HashMap::new();
        // Both endpoints far off to the same side → segment never crosses view.
        pos.insert(
            "a".to_owned(),
            Position {
                x: 50_000.0,
                y: 0.0,
                z: 0.0,
            },
        );
        pos.insert(
            "b".to_owned(),
            Position {
                x: 60_000.0,
                y: 0.0,
                z: 0.0,
            },
        );
        let layout = Layout::from_positions(pos);
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        assert!(out.spheres.is_empty(), "both spheres off-screen, culled");
        assert!(out.edges.is_empty(), "off-screen edge culled");
    }

    #[test]
    fn low_zoom_collapses_spheres_to_one_flat_layer() {
        let g = sample();
        let layout = g.scaffold_layout(); // tiny spiral near origin
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        // Zoom well below the LOD threshold; the wide view keeps every node
        // on-screen so we're testing LOD, not culling.
        let mut vp = viewport();
        vp.camera.zoom = 0.1;
        let out = Renderer2d::new().frame(&scene, &vp);
        assert_eq!(
            out.spheres.len(),
            4,
            "all nodes still on-screen at low zoom"
        );
        for s in &out.spheres {
            assert_eq!(s.layers.len(), 1, "flat-disc LOD = single layer");
        }
    }

    #[test]
    fn constant_node_is_desaturated_relative_to_function() {
        // A constant and a function from the same language compare: the
        // constant's core should be closer to grey.
        let g = WorkspaceGraph {
            nodes: vec![
                node("fnode", NodeKind::Function, "src/a.rs"),
                node("cnode", NodeKind::Constant, "src/a.rs"),
            ],
            edges: vec![],
            clusters: vec![],
        };
        let layout = g.scaffold_layout();
        let theme = Theme::load_v1().unwrap();
        let scene = Scene {
            graph: &g,
            layout: &layout,
            theme: &theme,
            mode: ViewMode::CodeGraph,
            scope: None,
        };
        let out = Renderer2d::new().frame(&scene, &viewport());
        let core = |id: &str| out.spheres.iter().find(|s| s.id == id).unwrap().layers[1].color;
        let spread = |c: Color| {
            let mx = c.r.max(c.g).max(c.b);
            let mn = c.r.min(c.g).min(c.b);
            mx - mn
        };
        // Desaturation narrows the channel spread.
        assert!(spread(core("cnode")) < spread(core("fnode")));
    }
}
