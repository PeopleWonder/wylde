//! The visual graph layer — Slice C-scaffold (Phase 3).
//!
//! Proves the **data → screen** path: load the active workspace's code graph
//! via `workspaces.graph` (Slice B), lay nodes out deterministically (no
//! physics), and render them as 2D radial-gradient spheres + per-rel-type
//! edges, all styled from the locked Visual Style v1 [`Theme`]. Pan + zoom
//! with the mouse; clicking a node records its id (placeholder for the real
//! click behaviour later slices add).
//!
//! Structure (Build Order §4):
//!   * [`model`]  — WHAT it is (pure data; mirrors the verb wire shape).
//!   * [`render`] — HOW it draws (pluggable [`render::Renderer`] trait; v1 =
//!     [`render::render_2d::Renderer2d`]; reads everything from the [`Theme`]).
//!   * [`ipc`]    — talks to `wylde-workspaces` with graceful degrade (OI-1).
//!
//! **Out of scope (later C-* sub-slices):** force-directed physics, real
//! layout backends, space-map navigation, clustering, the settings menu, the
//! vocabulary overlay. C-scaffold keeps to foundation + Theme + IPC + static
//! spheres.
//!
//! [`GraphView`] is the panel mount point — a gpui view the Workspaces
//! panel's Graph tab embeds.

pub mod ipc;
pub mod model;
pub mod render;

use std::rc::Rc;

use gpui::{
    canvas, div, point, prelude::*, px, size, App, AppContext, AsyncApp, Bounds, Context,
    ElementId, FontWeight, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Path, Pixels, Render, ScrollDelta, ScrollWheelEvent, SharedString, Window,
};
use wylde_theme::typography::{size as font_size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;
use ipc::{GraphFetchError, GraphLoad};
use model::{Layout, WorkspaceGraph};
use render::render_2d::Renderer2d;
use render::{Camera, Color, RenderOutput, Renderer, Scene, Theme, Viewport};

/// The canvas rectangle (window-absolute px) captured at paint time so mouse
/// handlers can project model↔screen for hit-testing.
#[derive(Clone, Copy, Debug, Default)]
struct CanvasRect {
    ox: f32,
    oy: f32,
    w: f32,
    h: f32,
}

/// In-flight drag-to-pan state.
#[derive(Clone, Copy, Debug)]
struct Drag {
    x: f32,
    y: f32,
    /// Set once the pointer moves past the click/drag threshold, so a release
    /// without movement is treated as a click (node hit-test) instead of a pan.
    moved: bool,
}

/// The graph panel view. Owns the loaded graph + its scaffold layout, the
/// theme, the camera, and transient interaction state.
pub struct GraphView {
    theme: Option<Rc<Theme>>,
    theme_error: Option<String>,
    graph: Rc<WorkspaceGraph>,
    layout: Rc<Layout>,
    camera: Camera,
    /// Whether the camera has been fitted to the graph yet (one-time on first
    /// non-empty paint).
    fitted: bool,
    /// Dark mode (default per Visual Style v1 implicit guidance).
    dark: bool,
    workspace_id: Option<String>,
    loading: bool,
    error: Option<GraphFetchError>,
    canvas: CanvasRect,
    drag: Option<Drag>,
    /// Last node the user clicked — surfaced in the header (placeholder for the
    /// real click behaviour future slices add) and logged to stderr.
    last_clicked: Option<String>,
}

/// Pointer movement (px) past which a press becomes a pan, not a click.
const DRAG_THRESHOLD: f32 = 3.0;

impl GraphView {
    pub fn new() -> Self {
        let (theme, theme_error) = match Theme::load_v1() {
            Ok(t) => (Some(Rc::new(t)), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            theme,
            theme_error,
            graph: Rc::new(WorkspaceGraph::default()),
            layout: Rc::new(Layout::default()),
            camera: Camera::default(),
            fitted: false,
            dark: true,
            workspace_id: None,
            loading: true,
            error: None,
            canvas: CanvasRect::default(),
            drag: None,
            last_clicked: None,
        }
    }

    /// Create the view entity and kick off the initial graph load.
    pub fn new_entity(cx: &mut App) -> gpui::Entity<Self> {
        cx.new(|cx| {
            let view = Self::new();
            Self::spawn_load(cx);
            view
        })
    }

    /// Load (or reload) the active workspace's graph. Degrades gracefully: on
    /// a service-unavailable error the last-known graph stays on screen under
    /// a banner with Retry (OI-1 / Plan v2 §7.3).
    pub fn spawn_load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = ipc::fetch_active_graph().await;
            let _ = this.update(app_cx, |view, cx| {
                view.loading = false;
                match outcome {
                    Ok(GraphLoad {
                        workspace_id,
                        graph,
                    }) => {
                        view.error = None;
                        view.workspace_id = workspace_id;
                        view.layout = Rc::new(graph.scaffold_layout());
                        view.graph = Rc::new(graph);
                        // Re-fit the camera to the freshly loaded graph.
                        view.fitted = false;
                    }
                    Err(e) => {
                        view.error = Some(e);
                        // Keep the last-known graph/layout for the degrade view.
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Build the viewport for a given canvas rect using the current camera.
    fn viewport(&self, rect: CanvasRect) -> Viewport {
        Viewport {
            origin_x: rect.ox,
            origin_y: rect.oy,
            width: rect.w,
            height: rect.h,
            camera: self.camera,
            dark: self.dark,
        }
    }

    /// Render the current scene into a [`RenderOutput`] for `rect`. `None` when
    /// the theme failed to load or there is nothing to draw. Shared by the
    /// canvas paint path and the click hit-test so both see identical geometry.
    fn render_output(&self, rect: CanvasRect, camera: Camera) -> Option<RenderOutput> {
        let theme = self.theme.as_ref()?;
        if self.graph.nodes.is_empty() {
            return None;
        }
        let scene = Scene {
            graph: &self.graph,
            layout: &self.layout,
            theme,
            mode: model::ViewMode::CodeGraph,
        };
        let mut vp = self.viewport(rect);
        vp.camera = camera;
        Some(Renderer2d::new().frame(&scene, &vp))
    }

    // ── Mouse handlers ──────────────────────────────────────────────────

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let units = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
        };
        if units.abs() < f32::EPSILON {
            return;
        }
        self.camera.zoom_by(1.15f32.powf(units));
        cx.notify();
    }

    fn on_down(&mut self, ev: &MouseDownEvent, _cx: &mut Context<Self>) {
        self.drag = Some(Drag {
            x: f32::from(ev.position.x),
            y: f32::from(ev.position.y),
            moved: false,
        });
    }

    fn on_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let (px_, py) = (f32::from(ev.position.x), f32::from(ev.position.y));
        let (dx, dy) = (px_ - drag.x, py - drag.y);
        if !drag.moved && dx.abs() + dy.abs() < DRAG_THRESHOLD {
            return;
        }
        drag.moved = true;
        drag.x = px_;
        drag.y = py;
        self.camera.pan_by(dx, dy);
        cx.notify();
    }

    fn on_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.moved {
            return; // it was a pan, not a click.
        }
        // A click — hit-test the current scene and record the node id.
        let cam = self.camera;
        if let Some(out) = self.render_output(self.canvas, cam) {
            if let Some(id) = out.hit_test(f32::from(ev.position.x), f32::from(ev.position.y)) {
                let id = id.to_owned();
                eprintln!("[workspaces.graph] clicked node {id}");
                self.last_clicked = Some(id);
                cx.notify();
            }
        }
    }
}

impl Default for GraphView {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for GraphView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Background colour from the theme (deep "space" void in dark mode);
        // a neutral fallback if the theme failed to parse.
        let bg = self
            .theme
            .as_ref()
            .map(|t| to_rgba(t.graph_panel.background.primary(self.dark)))
            .unwrap_or(gpui::Rgba {
                r: 0.04,
                g: 0.05,
                b: 0.08,
                a: 1.0,
            });

        let root_id: ElementId = ElementId::Name("workspaces-graph-canvas".into());
        let mut root = div()
            .id(root_id)
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(bg)
            .on_scroll_wheel(
                cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| this.on_scroll(ev, cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| this.on_down(ev, cx)),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| this.on_move(ev, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _w, cx| this.on_up(ev, cx)),
            );

        // The graph canvas (only when the theme is good and we have nodes).
        if self.theme.is_some() && !self.graph.nodes.is_empty() {
            root = root.child(self.canvas_element(cx));
        }

        root.child(self.overlay())
    }
}

impl GraphView {
    /// The low-level paint canvas. `prepaint` captures the canvas bounds (for
    /// hit-testing), applies the one-time camera fit, and builds the
    /// [`RenderOutput`]; `paint` translates it into gpui draw calls.
    fn canvas_element(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let graph = self.graph.clone();
        let layout = self.layout.clone();
        let theme = self.theme.clone();
        let camera = self.camera;
        let dark = self.dark;
        let fitted = self.fitted;

        canvas(
            move |bounds: Bounds<Pixels>, _window, app: &mut App| -> Option<RenderOutput> {
                let rect = CanvasRect {
                    ox: f32::from(bounds.origin.x),
                    oy: f32::from(bounds.origin.y),
                    w: f32::from(bounds.size.width),
                    h: f32::from(bounds.size.height),
                };
                let theme = theme?;

                // One-time fit on the first non-empty paint.
                let mut cam = camera;
                if !fitted && !graph.nodes.is_empty() && rect.w > 0.0 {
                    if let Some(bb) = graph.model_bounds(&layout) {
                        cam.zoom = Viewport::fit_zoom(bb, rect.w, rect.h);
                    }
                }

                // Persist the canvas rect + fitted camera back onto the view
                // (no notify — this only informs the *next* interaction).
                entity.update(app, |view, _| {
                    view.canvas = rect;
                    if !view.fitted && cam != view.camera {
                        view.camera = cam;
                    }
                    view.fitted = true;
                });

                let scene = Scene {
                    graph: &graph,
                    layout: &layout,
                    theme: &theme,
                    mode: model::ViewMode::CodeGraph,
                };
                let vp = Viewport {
                    origin_x: rect.ox,
                    origin_y: rect.oy,
                    width: rect.w,
                    height: rect.h,
                    camera: cam,
                    dark,
                };
                Some(Renderer2d::new().frame(&scene, &vp))
            },
            move |_bounds, output: Option<RenderOutput>, window, _app| {
                if let Some(out) = output {
                    paint_graph(window, &out);
                }
            },
        )
        .absolute()
        .size_full()
    }

    /// The status overlay (top strip): workspace name + counts + zoom + last
    /// clicked, plus the loading / empty / degrade states.
    fn overlay(&self) -> gpui::Div {
        let mut col = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3();

        // Status line.
        let title = self.workspace_id.clone().unwrap_or_else(|| "—".to_owned());
        let status = format!(
            "Graph · {title} · {} nodes · {} edges · zoom {:.0}%",
            self.graph.nodes.len(),
            self.graph.edges.len(),
            self.camera.zoom * 100.0
        );
        col = col.child(overlay_text(status, font_size::XS, weight::SEMIBOLD));

        if let Some(id) = &self.last_clicked {
            col = col.child(overlay_text(
                format!("Selected: {id}"),
                font_size::MICRO,
                weight::REGULAR,
            ));
        }

        if let Some(err) = &self.theme_error {
            col = col.child(overlay_text(
                format!("Visual style failed to load: {err}"),
                font_size::XS,
                weight::REGULAR,
            ));
            return col;
        }

        if self.loading {
            col = col.child(overlay_text(
                "Loading graph…".to_owned(),
                font_size::SM,
                weight::REGULAR,
            ));
        } else if let Some(err) = &self.error {
            // Graceful degrade banner (OI-1).
            let msg = if err.is_service_unavailable() {
                "Workspaces service unavailable — showing last-known graph. Start the \
                 workspaces service, then click to retry."
                    .to_owned()
            } else {
                err.message().to_owned()
            };
            col = col.child(overlay_text(msg, font_size::XS, weight::REGULAR));
        } else if self.workspace_id.is_none() {
            col = col.child(overlay_text(
                "No active workspace — add one in the Registry tab to see its code graph."
                    .to_owned(),
                font_size::SM,
                weight::REGULAR,
            ));
        } else if self.graph.nodes.is_empty() {
            col = col.child(overlay_text(
                "This workspace has no indexed code graph yet — re-index it in the Registry tab."
                    .to_owned(),
                font_size::SM,
                weight::REGULAR,
            ));
        }

        col
    }
}

/// One line of overlay text.
fn overlay_text(s: String, sz: f32, w: u16) -> gpui::Div {
    use wylde_theme::colors::TEXT_PRIMARY;
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(sz))
        .text_color(gpui::rgb(pack(TEXT_PRIMARY)))
        .font_weight(FontWeight(w as f32))
        .child(SharedString::from(s))
}

/// Translate a renderer [`RenderOutput`] into gpui paint calls. Spheres are
/// concentric filled circles (the radial-gradient fake); edges are thin filled
/// quads via [`Path`].
fn paint_graph(window: &mut Window, out: &RenderOutput) {
    // Edges first (under the spheres).
    for e in &out.edges {
        paint_line(window, e.x0, e.y0, e.x1, e.y1, e.thickness, e.color);
    }
    // Spheres, layer by layer (rim → core → specular), border on the rim.
    for s in &out.spheres {
        for (i, layer) in s.layers.iter().enumerate() {
            let r = layer.radius.max(0.5);
            let b = circle_bounds(s.cx + layer.dx, s.cy + layer.dy, r);
            let (bw, bc) = if i == 0 {
                (px(s.border_width), to_hsla(s.border_color))
            } else {
                (px(0.0), to_hsla(layer.color))
            };
            window.paint_quad(gpui::quad(
                b,
                r, // corner radius = radius → a circle
                to_rgba(layer.color),
                bw,
                bc,
                gpui::BorderStyle::Solid,
            ));
        }
    }
}

/// Square bounds for a circle of radius `r` centred on `(cx, cy)`.
fn circle_bounds(cx: f32, cy: f32, r: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(cx - r), px(cy - r)),
        size: size(px(r * 2.0), px(r * 2.0)),
    }
}

/// Paint a line segment as a thin filled quad (rotated rectangle) via a
/// triangle-fan [`Path`].
fn paint_line(window: &mut Window, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, c: Color) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return;
    }
    let half = (thickness * 0.5).max(0.3);
    // Unit normal.
    let (nx, ny) = (-dy / len * half, dx / len * half);
    let a = point(px(x0 + nx), px(y0 + ny));
    let b = point(px(x1 + nx), px(y1 + ny));
    let cc = point(px(x1 - nx), px(y1 - ny));
    let d = point(px(x0 - nx), px(y0 - ny));
    let mut path = Path::new(a);
    path.line_to(b);
    path.line_to(cc);
    path.line_to(d);
    window.paint_path(path, to_rgba(c));
}

/// `Color` → gpui `Rgba` (both are 0..=1 RGBA; just a field copy).
fn to_rgba(c: Color) -> gpui::Rgba {
    gpui::Rgba {
        r: c.r,
        g: c.g,
        b: c.b,
        a: c.a,
    }
}

/// `Color` → gpui `Hsla` (for border colours, which take `Into<Hsla>`).
fn to_hsla(c: Color) -> gpui::Hsla {
    to_rgba(c).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Edge, Node, NodeKind, Position, RelType};

    fn node(id: &str, file: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            file: file.to_owned(),
            line: 0,
            position: Position::default(),
            style: Default::default(),
        }
    }

    fn view_with_graph(g: WorkspaceGraph) -> GraphView {
        let mut v = GraphView::new();
        v.layout = Rc::new(g.scaffold_layout());
        v.graph = Rc::new(g);
        v.loading = false;
        v.workspace_id = Some("demo".to_owned());
        v
    }

    #[test]
    fn new_view_loads_theme_and_starts_loading() {
        let v = GraphView::new();
        assert!(v.theme.is_some(), "locked theme parses at construction");
        assert!(v.theme_error.is_none());
        assert!(v.loading);
        assert!(v.graph.nodes.is_empty());
    }

    #[test]
    fn render_output_builds_scene_from_real_data() {
        let g = WorkspaceGraph {
            nodes: vec![node("a", "src/a.rs"), node("b", "src/b.rs")],
            edges: vec![Edge {
                src: "a".to_owned(),
                dst: "b".to_owned(),
                rel_type: RelType::Calls,
                weight: 1.0,
            }],
            clusters: vec![],
        };
        let v = view_with_graph(g);
        let rect = CanvasRect {
            ox: 0.0,
            oy: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let out = v.render_output(rect, Camera::default()).expect("render");
        assert_eq!(out.spheres.len(), 2);
        assert!(!out.edges.is_empty());
    }

    #[test]
    fn click_hit_test_resolves_a_node_at_its_centre() {
        let g = WorkspaceGraph {
            nodes: vec![node("solo", "src/a.rs")],
            edges: vec![],
            clusters: vec![],
        };
        let v = view_with_graph(g);
        let rect = CanvasRect {
            ox: 0.0,
            oy: 0.0,
            w: 400.0,
            h: 400.0,
        };
        let out = v.render_output(rect, Camera::default()).unwrap();
        // The single node sits at the model origin → canvas centre.
        let (cx, cy) = (rect.ox + rect.w / 2.0, rect.oy + rect.h / 2.0);
        assert_eq!(out.hit_test(cx, cy), Some("solo"));
    }

    #[test]
    fn empty_graph_yields_no_render_output() {
        let v = GraphView::new();
        let rect = CanvasRect {
            ox: 0.0,
            oy: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert!(v.render_output(rect, Camera::default()).is_none());
    }

    #[test]
    fn camera_zoom_clamps_via_scroll_math() {
        // Pure camera math (no gpui context needed).
        let mut cam = Camera::default();
        for _ in 0..200 {
            cam.zoom_by(1.15);
        }
        assert!(cam.zoom <= render::viewport::MAX_ZOOM);
    }

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<GraphView>();
    }
}
