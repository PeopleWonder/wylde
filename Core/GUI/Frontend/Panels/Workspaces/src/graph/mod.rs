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
//!   * [`layout`] / [`physics`] — WHERE things go / HOW they move
//!     (C-layout / C-physics); the animated swap driver is
//!     `transition_driver` (private).
//!   * [`navigation`] — WHERE the user is looking (Slice C-navigation:
//!     space-map zoom, breadcrumb, exit edges).
//!   * [`cluster`] — WHICH nodes group (Slice C-cluster: threshold-driven
//!     auto-clustering + expand-in-place).
//!   * `paint` (private) — gpui draw-call plumbing for the renderer output.
//!
//! [`GraphView`] is the panel mount point — a gpui view the Workspaces
//! panel's Graph tab embeds.

pub mod cluster;
pub mod ipc;
pub mod layout;
pub mod model;
pub mod navigation;
mod paint;
pub mod physics;
pub mod render;
mod transition_driver;

// Integration + perf suite (Build Order §4 file tree → `graph/tests/`). A
// `#[path]` module so it can live in the spec's `tests/` directory without
// colliding with this file's own inline `tests` module (the GraphView unit
// tests at the bottom).
#[cfg(test)]
#[path = "tests/physics_tests.rs"]
mod physics_tests;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    canvas, div, prelude::*, px, App, AppContext, AsyncApp, Bounds, Context, ElementId,
    FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollWheelEvent, SharedString, Window,
};
use wylde_theme::typography::{size as font_size, weight};

use cluster::{ClusterView, Override};
use ipc::{GraphFetchError, GraphLoad};
use layout::{CubicBezier, ForceDirected, LayoutKind};
use model::{Layout, WorkspaceGraph};
use navigation::input::Drag;
use navigation::transition::ActiveCameraTween;
use navigation::{append_exit_stubs, compute_exit_edges, NavAction, Navigator};
use paint::{overlay_text, paint_graph, to_rgba};
use physics::{ActiveRegion, PhysicsConfig, PhysicsHandle, PositionFrame};
use render::render_2d::Renderer2d;
use render::{Camera, RenderOutput, Renderer, Scene, Theme, Viewport};
use transition_driver::ActiveTransition;

/// The canvas rectangle (window-absolute px) captured at paint time so mouse
/// handlers can project model↔screen for hit-testing.
#[derive(Clone, Copy, Debug, Default)]
struct CanvasRect {
    ox: f32,
    oy: f32,
    w: f32,
    h: f32,
}

/// An open right-click menu over a cluster (C-cluster): Expand Cluster on a
/// folded sphere, Collapse Cluster on a member of an expandable cluster.
pub(crate) struct ClusterMenu {
    /// Click position, window px.
    x: f32,
    y: f32,
    cluster_id: String,
    /// `true` → the target is folded (menu offers Expand); `false` → expanded
    /// (menu offers Collapse).
    folded: bool,
}

/// The graph panel view. Owns the loaded graph + its scaffold layout, the
/// theme, the camera, and transient interaction state.
pub struct GraphView {
    theme: Option<Rc<Theme>>,
    theme_error: Option<String>,
    graph: Rc<WorkspaceGraph>,
    layout: Rc<Layout>,
    /// The off-thread physics worker driving `layout`. `None` until the first
    /// graph loads, when the graph is empty, or while a deterministic layout
    /// (hierarchical / stable-grid) is shown. Dropping it shuts the worker
    /// down; replacing it on reload/swap swaps in a fresh simulation.
    physics: Option<PhysicsHandle>,
    /// Which layout backend is active (default: force-directed). Cycled by
    /// `Ctrl+Shift+L`.
    current_layout: LayoutKind,
    /// Per-workspace remembered layout choice. **Panel-local + in-memory only**
    /// in v1: layout persistence proper (the `graph_profiles.json` /
    /// `workspace_pointer` system) lands in C-settings, so extending the
    /// `wylde-workspaces` registry now would duplicate work that slice owns.
    /// Keyed by workspace id; consulted on (re)load.
    layout_cache: HashMap<String, LayoutKind>,
    /// In-flight animated layout swap; `Some` only while the 500 ms tween runs.
    /// While set, the physics subscription ignores worker frames (the animation
    /// owns positions).
    transition: Option<ActiveTransition>,
    /// Space-map scope state (C-navigation): which cluster the user is inside,
    /// the way back out, and the navigation knobs.
    navigator: Navigator,
    /// In-flight camera tween (zoom-into-cluster / zoom-out). Camera-only —
    /// node positions and the physics worker are untouched while it runs.
    camera_transition: Option<ActiveCameraTween>,
    /// Exit-edge jump second phase: the cluster to enter once the in-flight
    /// zoom-out tween lands.
    pending_enter: Option<String>,
    /// Auto-clustering + expand-in-place state (C-cluster).
    cluster_view: ClusterView,
    /// Open right-click menu (Expand / Collapse Cluster), if any.
    cluster_menu: Option<ClusterMenu>,
    /// Focus handle so the canvas can receive `Ctrl+Shift+L`. Created lazily in
    /// `render` (no gpui context at `new()` / in unit tests).
    focus: Option<FocusHandle>,
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
            physics: None,
            current_layout: LayoutKind::default(),
            layout_cache: HashMap::new(),
            transition: None,
            navigator: Navigator::default(),
            camera_transition: None,
            pending_enter: None,
            cluster_view: ClusterView::default(),
            cluster_menu: None,
            focus: None,
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
                        view.graph = Rc::new(graph);
                        // A reload cancels any in-flight swap from the old graph
                        // and drops the space-map scope (the clusters may be gone).
                        view.transition = None;
                        view.navigator.reset();
                        view.camera_transition = None;
                        view.pending_enter = None;
                        // Re-run the one-time cluster assignment + auto-fold
                        // selection; the snap to the post-fit zoom happens at
                        // the first paint (see canvas_element).
                        view.cluster_view.rebuild(&view.graph, view.camera.zoom);
                        view.cluster_menu = None;
                        // Apply the per-workspace remembered layout (default:
                        // force-directed). Deterministic layouts compute their
                        // final positions and leave the physics worker paused;
                        // force-directed warm-starts (depth-banded) and spins up
                        // the worker to refine off-thread.
                        let kind = view
                            .workspace_id
                            .as_ref()
                            .and_then(|id| view.layout_cache.get(id).copied())
                            .unwrap_or_default();
                        view.current_layout = kind;
                        view.layout = Rc::new(kind.compute_positions(view.graph.as_ref()));
                        // Re-fit the camera to the freshly loaded graph.
                        view.fitted = false;
                        if kind.is_physics() {
                            view.start_physics(cx);
                        } else {
                            view.physics = None;
                        }
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

    /// Build the off-thread physics worker for the current graph. `seed`, when
    /// given, becomes the worker's initial positions (used to resume physics
    /// after an animated swap lands the nodes at the target); `None` uses the
    /// depth-banded warm start. Returns `None` for an empty graph. Does **not**
    /// subscribe the view — pair with [`subscribe_physics`](Self::subscribe_physics).
    fn spawn_worker(&self, seed: Option<&Layout>) -> Option<PhysicsHandle> {
        if self.graph.nodes.is_empty() {
            return None;
        }
        let fd = ForceDirected::default();
        let cfg = PhysicsConfig::default();
        let engine = match seed {
            Some(s) => fd.build_engine_with_seed(self.graph.as_ref(), cfg, s),
            None => fd.build_engine(self.graph.as_ref(), cfg),
        };
        Some(PhysicsHandle::spawn(engine))
    }

    /// Subscribe the view to the current worker's latched frames. Re-renders on
    /// each new frame (the render budget is untouched — we only read the most
    /// recent positions). While an animated swap is running, frames are ignored
    /// so the tween owns positions. No-op when there is no worker.
    fn subscribe_physics(&self, cx: &mut Context<Self>) {
        let Some(handle) = &self.physics else {
            return;
        };
        let mut rx = handle.receiver();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            while rx.changed().await.is_ok() {
                let frame: Arc<PositionFrame> = rx.borrow_and_update().clone();
                let alive = this
                    .update(app_cx, |view, cx| {
                        if view.transition.is_none() {
                            view.layout = Rc::new(frame.to_layout());
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break; // the view is gone — stop subscribing.
                }
            }
        })
        .detach();
    }

    /// Spawn (or respawn) the off-thread physics worker for the current graph
    /// and subscribe the view to it. Dropping the old handle stops the previous
    /// worker; the subscription loop ends when its worker's sender drops.
    fn start_physics(&mut self, cx: &mut Context<Self>) {
        self.physics = None; // drop any prior worker before building a new one
        self.physics = self.spawn_worker(None);
        self.subscribe_physics(cx);
    }

    /// Tell the worker the current visible model-space rect so it can cull
    /// off-screen nodes (and resume — a camera move is a resume trigger). A
    /// no-op when there is no worker.
    fn push_viewport(&self) {
        if let Some(h) = &self.physics {
            let (x0, y0, x1, y1) = self.viewport(self.canvas).visible_model_rect();
            h.set_region(Some(ActiveRegion {
                min_x: x0,
                min_y: y0,
                max_x: x1,
                max_y: y1,
            }));
        }
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
    /// The graph + layout the render paths draw: the cluster display
    /// transform (C-cluster) when folds are active and the view is not
    /// scoped, else the real ones. Scoped views bypass clustering — the
    /// scope filter already isolates one cluster's members.
    fn display_graph_layout(&self) -> (Rc<WorkspaceGraph>, Rc<Layout>) {
        if !self.navigator.is_scoped() {
            if let Some((g, l)) = self.cluster_view.apply(&self.graph, &self.layout) {
                return (Rc::new(g), Rc::new(l));
            }
        }
        (self.graph.clone(), self.layout.clone())
    }

    /// Expanded-in-place boundary rects (model space) for the current frame;
    /// empty when scoped.
    fn boundary_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        if self.navigator.is_scoped() {
            return Vec::new();
        }
        self.cluster_view
            .expanded_boundaries(&self.layout)
            .into_iter()
            .map(|(_, bb)| bb)
            .collect()
    }

    fn render_output(&self, rect: CanvasRect, camera: Camera) -> Option<RenderOutput> {
        let theme = self.theme.as_ref()?;
        if self.graph.nodes.is_empty() {
            return None;
        }
        let members = self.navigator.members();
        let (graph, layout) = self.display_graph_layout();
        let scene = Scene {
            graph: &graph,
            layout: &layout,
            theme,
            mode: model::ViewMode::CodeGraph,
            scope: members.as_deref(),
        };
        let mut vp = self.viewport(rect);
        vp.camera = camera;
        let mut out = Renderer2d::new().frame(&scene, &vp);
        // Scoped: edges that leave the cluster fade out as exit stubs
        // (computed over the REAL graph — the scope filter hides the rest).
        if let Some(m) = members.as_deref() {
            let xe = compute_exit_edges(
                &self.graph,
                &self.layout,
                m,
                &vp,
                theme.graph_panel.exit_edges.fade_distance_px,
                self.navigator.config.max_exit_labels,
            );
            append_exit_stubs(
                &mut out,
                &xe.stubs,
                theme,
                self.dark,
                self.navigator.config.exit_stub_segments,
            );
        }
        // Expanded-in-place cluster boundaries (Theme `cluster_boundary`).
        append_boundaries(&mut out, &self.boundary_rects(), theme, self.dark, &vp);
        Some(out)
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

        // Focus handle (created lazily) so the canvas can capture
        // `Ctrl+Shift+L`. Clicking the canvas focuses it.
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();

        // The interactive graph area (canvas + overlay + exit chips). The
        // breadcrumb bar sits above it in normal flow, so mouse handlers live
        // here rather than on the root.
        let content_id: ElementId = ElementId::Name("workspaces-graph-canvas".into());
        let mut content = div()
            .id(content_id)
            .track_focus(&focus)
            .relative()
            .flex_1()
            .w_full()
            .overflow_hidden()
            .bg(bg)
            .on_scroll_wheel(
                cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| this.on_scroll(ev, cx)),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, window, cx| this.on_key(ev, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    // Take keyboard focus so the layout-cycle keybind works.
                    if let Some(f) = this.focus.clone() {
                        f.focus(window, cx);
                    }
                    this.on_down(ev, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| this.on_move(ev, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _w, cx| this.on_up(ev, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| this.on_right_click(ev, cx)),
            );

        // The graph canvas (only when the theme is good and we have nodes).
        if self.theme.is_some() && !self.graph.nodes.is_empty() {
            content = content.child(self.canvas_element(cx));
        }

        // Exit-edge destination chips (scoped space-map only, C-navigation).
        content = self.exit_label_chips(content, cx);
        // Right-click cluster menu (C-cluster).
        if let Some(menu) = self.cluster_menu_element(cx) {
            content = content.child(menu);
        }
        content = content.child(self.overlay());

        // Root: breadcrumb bar (Theme `graph_panel.breadcrumb_bar`) over the
        // graph area.
        let root_id: ElementId = ElementId::Name("workspaces-graph-root".into());
        let mut root = div().id(root_id).size_full().flex().flex_col().bg(bg);
        if let Some(theme) = self.theme.clone() {
            if !self.graph.nodes.is_empty() {
                root = root.child(self.breadcrumb_bar(&theme, cx));
            }
        }
        root.child(content)
    }
}

impl GraphView {
    /// Append the scoped frame's exit-edge destination chips to `content` as
    /// absolutely-positioned, clickable elements (Theme
    /// `graph_panel.exit_edges` label styling). A chip with a known
    /// destination cluster jumps there (zoom-out → re-zoom-in); a chip for an
    /// unclustered destination is inert context.
    fn exit_label_chips(
        &self,
        mut content: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (Some(theme), Some(members)) = (self.theme.as_ref(), self.navigator.members()) else {
            return content;
        };
        if self.canvas.w <= 0.0 {
            return content; // no painted canvas to anchor chips to yet
        }
        let xe = compute_exit_edges(
            &self.graph,
            &self.layout,
            &members,
            &self.viewport(self.canvas),
            theme.graph_panel.exit_edges.fade_distance_px,
            self.navigator.config.max_exit_labels,
        );
        let chip_bg = to_rgba(theme.graph_panel.exit_edges.label_background(self.dark));
        let chip_fg = to_rgba(theme.graph_panel.exit_edges.label_text(self.dark));
        let font = theme.graph_panel.exit_edges.label_font_size_px;
        for (i, label) in xe.labels.into_iter().enumerate() {
            let mut chip = div()
                .id(("graph-exit-label", i))
                .absolute()
                .left(px(label.x - self.canvas.ox))
                .top(px(label.y - self.canvas.oy))
                .px_1()
                .bg(chip_bg)
                .text_size(px(font))
                .text_color(chip_fg)
                .child(SharedString::from(label.text.clone()));
            if let Some(target) = label.target_cluster {
                chip = chip.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                        this.apply_nav_action(NavAction::JumpToCluster(target.clone()), cx);
                    }),
                );
            }
            content = content.child(chip);
        }
        content
    }

    // ── Clustering (Slice C-cluster) ─────────────────────────────────────

    /// The expand-in-place animation params, read FROM the Theme
    /// (`animations.cluster_expand_in_place`); locked-spec fallback.
    fn cluster_anim(&self) -> (f32, CubicBezier) {
        self.theme
            .as_ref()
            .and_then(|t| t.animation("cluster_expand_in_place"))
            .map(|a| (a.duration_ms, CubicBezier::from_array(a.easing)))
            .unwrap_or((
                cluster::expand::EXPAND_FALLBACK_MS,
                cluster::expand::EXPAND_FALLBACK_EASING,
            ))
    }

    /// Re-resolve the fold set against the current zoom and overrides; arms
    /// expand/collapse tweens and starts the animation loop when anything
    /// flips.
    pub(in crate::graph) fn sync_clusters(&mut self, cx: &mut Context<Self>) {
        let anim = self.cluster_anim();
        if self
            .cluster_view
            .sync(self.camera.zoom, Instant::now(), anim)
        {
            self.spawn_cluster_driver(cx);
        }
    }

    /// Apply a right-click menu choice: override the cluster's fold state and
    /// animate to it.
    pub(in crate::graph) fn toggle_cluster_fold(
        &mut self,
        cluster_id: &str,
        expand: bool,
        cx: &mut Context<Self>,
    ) {
        let ov = if expand {
            Override::Expanded
        } else {
            Override::Collapsed
        };
        self.cluster_view.set_override(cluster_id, ov);
        self.cluster_menu = None;
        self.sync_clusters(cx);
        cx.notify();
    }

    /// Drive the expand/collapse tweens at ~60 fps until they all land
    /// (same main-thread pattern as the layout-swap and camera drivers).
    fn spawn_cluster_driver(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
            app_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let running = this.update(app_cx, |view, cx| {
                let running = view.cluster_view.advance(Instant::now());
                cx.notify();
                running
            });
            match running {
                Ok(true) => continue,
                _ => break,
            }
        })
        .detach();
    }

    /// The right-click cluster menu (Theme `ui_chrome.context_menu`), if open.
    fn cluster_menu_element(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let menu = self.cluster_menu.as_ref()?;
        let theme = self.theme.as_ref()?;
        let m = &theme.ui_chrome.context_menu;
        let label = if menu.folded {
            "Expand Cluster"
        } else {
            "Collapse Cluster"
        };
        let target = menu.cluster_id.clone();
        let expand = menu.folded;
        Some(
            div()
                .id("graph-cluster-menu")
                .absolute()
                .left(px(menu.x - self.canvas.ox))
                .top(px(menu.y - self.canvas.oy))
                .bg(to_rgba(m.background(self.dark)))
                .rounded(px(m.border_radius_px))
                .overflow_hidden()
                .text_size(px(m.font_size_px))
                // The menu section carries no text colour of its own; the
                // chrome text pair (breadcrumb bar) is the same palette.
                .text_color(to_rgba(theme.graph_panel.breadcrumb_bar.text(self.dark)))
                .child(
                    div()
                        .id("graph-cluster-menu-item")
                        .h(px(m.item_height_px))
                        .px(px(m.item_padding_px))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.toggle_cluster_fold(&target, expand, cx);
                            }),
                        )
                        .child(SharedString::from(label)),
                ),
        )
    }
}

/// Project model-space cluster boundary rects into the frame's draw list
/// using the Theme `cluster_boundary` styling.
fn append_boundaries(
    out: &mut RenderOutput,
    rects: &[(f32, f32, f32, f32)],
    theme: &Theme,
    dark: bool,
    vp: &Viewport,
) {
    let cb = &theme.graph_panel.cluster_boundary;
    for bb in rects {
        let (x0, y0) = vp.model_to_screen(model::Position {
            x: bb.0,
            y: bb.1,
            z: 0.0,
        });
        let (x1, y1) = vp.model_to_screen(model::Position {
            x: bb.2,
            y: bb.3,
            z: 0.0,
        });
        out.outlines.push(render::OutlineRect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0.0),
            h: (y1 - y0).max(0.0),
            corner_radius: cb.corner_radius_px,
            fill: cb.fill(dark),
            border: cb.border(dark),
            border_width: cb.border_width_px,
        });
    }
}

impl GraphView {
    /// The low-level paint canvas. `prepaint` captures the canvas bounds (for
    /// hit-testing), applies the one-time camera fit, and builds the
    /// [`RenderOutput`]; `paint` translates it into gpui draw calls.
    fn canvas_element(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        // Draw the display graph (cluster transform applied when folds are
        // active and the view is unscoped — see `display_graph_layout`).
        let (graph, layout) = self.display_graph_layout();
        let boundaries = self.boundary_rects();
        let theme = self.theme.clone();
        let camera = self.camera;
        let dark = self.dark;
        let fitted = self.fitted;
        let members = self.navigator.members();
        let stub_segments = self.navigator.config.exit_stub_segments;
        let max_exit_labels = self.navigator.config.max_exit_labels;

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
                    if !view.fitted {
                        if cam != view.camera {
                            view.camera = cam;
                        }
                        // First paint: snap the auto-fold set to the fitted
                        // zoom (nothing on screen yet to animate from).
                        view.cluster_view.snap_to(cam.zoom);
                    }
                    view.fitted = true;
                });

                let scene = Scene {
                    graph: &graph,
                    layout: &layout,
                    theme: &theme,
                    mode: model::ViewMode::CodeGraph,
                    scope: members.as_deref(),
                };
                let vp = Viewport {
                    origin_x: rect.ox,
                    origin_y: rect.oy,
                    width: rect.w,
                    height: rect.h,
                    camera: cam,
                    dark,
                };
                let mut out = Renderer2d::new().frame(&scene, &vp);
                // Scoped: append the exit-edge fade stubs (C-navigation).
                if let Some(m) = members.as_deref() {
                    let xe = compute_exit_edges(
                        &graph,
                        &layout,
                        m,
                        &vp,
                        theme.graph_panel.exit_edges.fade_distance_px,
                        max_exit_labels,
                    );
                    append_exit_stubs(&mut out, &xe.stubs, &theme, dark, stub_segments);
                }
                // Expanded-in-place cluster boundaries (C-cluster).
                append_boundaries(&mut out, &boundaries, &theme, dark, &vp);
                Some(out)
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
        let layout = if self.transition.is_some() {
            format!("{} (swapping…)", self.current_layout.label())
        } else {
            self.current_layout.label().to_owned()
        };
        let status = format!(
            "Graph · {title} · {} nodes · {} edges · zoom {:.0}% · {layout}",
            self.graph.nodes.len(),
            self.graph.edges.len(),
            self.camera.zoom * 100.0
        );
        col = col.child(overlay_text(status, font_size::XS, weight::SEMIBOLD));
        let hint = if self.navigator.is_scoped() {
            "Scroll — zoom · Esc — zoom out · Ctrl+Shift+L — cycle layout"
        } else {
            "Scroll — zoom into clusters · Ctrl+Shift+L — cycle layout"
        };
        col = col.child(overlay_text(
            hint.to_owned(),
            font_size::MICRO,
            weight::REGULAR,
        ));

        if self.cluster_view.is_active() && !self.navigator.is_scoped() {
            let folded = self.cluster_view.folded_count();
            if folded > 0 {
                col = col.child(overlay_text(
                    format!("{folded} clusters folded — zoom in or right-click a sphere to expand"),
                    font_size::MICRO,
                    weight::REGULAR,
                ));
            }
        }

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

#[cfg(test)]
mod tests {
    use super::transition_driver::TransitionStep;
    use super::*;
    use model::{Edge, Node, NodeKind, Position, RelType};
    use std::time::{Duration, Instant};

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

    // ── Layout swap (Slice C-layout) ─────────────────────────────────────

    fn swap_graph() -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![
                node("a", "svc-one/src/a.rs"),
                node("b", "svc-one/src/b.rs"),
                node("c", "svc-two/src/c.rs"),
            ],
            edges: vec![Edge {
                src: "a".to_owned(),
                dst: "b".to_owned(),
                rel_type: RelType::Calls,
                weight: 1.0,
            }],
            clusters: vec![],
        }
    }

    #[test]
    fn new_view_defaults_to_force_directed() {
        let v = GraphView::new();
        assert_eq!(v.current_layout, LayoutKind::ForceDirected);
        assert!(v.transition.is_none());
    }

    #[test]
    fn swap_to_same_layout_is_a_noop() {
        let mut v = view_with_graph(swap_graph());
        assert!(!v.begin_layout_swap(LayoutKind::ForceDirected, Instant::now()));
        assert!(v.transition.is_none());
    }

    #[test]
    fn force_to_hierarchical_animates_from_current_to_target() {
        let mut v = view_with_graph(swap_graph());
        let from = (*v.layout).clone();
        let to = LayoutKind::Hierarchical.compute_positions(v.graph.as_ref());
        let base = Instant::now();

        assert!(v.begin_layout_swap(LayoutKind::Hierarchical, base));
        assert_eq!(v.current_layout, LayoutKind::Hierarchical);
        assert!(v.transition.is_some());

        // t = 0 → the captured `from` positions.
        let _ = v.advance_transition(base);
        let p0 = v.layout.get("a").unwrap();
        let f = from.get("a").unwrap();
        assert!((p0.x - f.x).abs() < 1e-2 && (p0.y - f.y).abs() < 1e-2);

        // t = 250 ms → strictly between from and to (the node actually moves).
        let _ = v.advance_transition(base + Duration::from_millis(250));
        let pmid = v.layout.get("a").unwrap();

        // t = 500 ms → the target, transition cleared, physics paused.
        let step = v.advance_transition(base + Duration::from_millis(500));
        assert!(matches!(step, TransitionStep::Completed));
        assert!(v.transition.is_none());
        assert!(
            v.physics.is_none(),
            "deterministic target keeps physics paused"
        );
        let pend = v.layout.get("a").unwrap();
        let t = to.get("a").unwrap();
        assert!((pend.x - t.x).abs() < 1e-2 && (pend.y - t.y).abs() < 1e-2);

        // The midpoint genuinely lies along the path (not pinned at an endpoint)
        // when from ≠ to.
        let total = ((t.x - f.x).powi(2) + (t.y - f.y).powi(2)).sqrt();
        if total > 1.0 {
            assert!(
                pmid.x != p0.x || pmid.y != p0.y,
                "node advanced by the midpoint"
            );
        }
    }

    #[test]
    fn hierarchical_to_grid_keeps_physics_paused() {
        let mut v = view_with_graph(swap_graph());
        let base = Instant::now();

        // Into hierarchical (deterministic) first.
        assert!(v.begin_layout_swap(LayoutKind::Hierarchical, base));
        let _ = v.advance_transition(base + Duration::from_millis(600));
        assert!(v.physics.is_none());
        assert_eq!(v.current_layout, LayoutKind::Hierarchical);

        // Then hierarchical → stable-grid: still deterministic, still paused.
        let base2 = base + Duration::from_millis(700);
        assert!(v.begin_layout_swap(LayoutKind::StableGrid, base2));
        assert!(
            v.physics.is_none(),
            "paused for the duration of the swap too"
        );
        let step = v.advance_transition(base2 + Duration::from_millis(600));
        assert!(matches!(step, TransitionStep::Completed));
        assert!(
            v.physics.is_none(),
            "grid is deterministic → physics paused"
        );
        assert_eq!(v.current_layout, LayoutKind::StableGrid);
    }

    #[test]
    fn grid_back_to_force_resumes_physics() {
        let mut v = view_with_graph(swap_graph());
        let base = Instant::now();

        // Park on a deterministic layout (physics paused).
        assert!(v.begin_layout_swap(LayoutKind::StableGrid, base));
        let _ = v.advance_transition(base + Duration::from_millis(600));
        assert!(v.physics.is_none());

        // Swap back to force-directed → the worker resumes on completion.
        let base2 = base + Duration::from_millis(700);
        assert!(v.begin_layout_swap(LayoutKind::ForceDirected, base2));
        assert!(v.physics.is_none(), "no worker mid-swap");
        let step = v.advance_transition(base2 + Duration::from_millis(600));
        assert!(matches!(step, TransitionStep::Completed));
        assert_eq!(v.current_layout, LayoutKind::ForceDirected);
        let handle = v.physics.as_ref().expect("physics resumed");
        // The worker is seeded from the swap result (exact warm-start seeding is
        // proven race-free by `force_directed::build_engine_with_seed_*`); here
        // we assert the worker exists with the full node set.
        assert_eq!(handle.latest().positions.len(), v.graph.nodes.len());
    }

    #[test]
    fn swap_remembers_choice_per_workspace() {
        let mut v = view_with_graph(swap_graph()); // workspace_id = "demo"
        let base = Instant::now();
        assert!(v.begin_layout_swap(LayoutKind::Hierarchical, base));
        assert_eq!(
            v.layout_cache.get("demo").copied(),
            Some(LayoutKind::Hierarchical),
            "layout choice cached for the active workspace"
        );
    }

    // ── Space-map navigation (Slice C-navigation) ────────────────────────

    use model::Cluster;
    use navigation::transition::CameraStep;

    /// Two clusters: alpha = {a, b}, beta = {c}; a→c crosses between them.
    fn nav_graph() -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![
                node("a", "ws/alpha/a.rs"),
                node("b", "ws/alpha/b.rs"),
                node("c", "ws/beta/c.rs"),
            ],
            edges: vec![
                Edge {
                    src: "a".to_owned(),
                    dst: "b".to_owned(),
                    rel_type: RelType::Calls,
                    weight: 1.0,
                },
                Edge {
                    src: "a".to_owned(),
                    dst: "c".to_owned(),
                    rel_type: RelType::Calls,
                    weight: 1.0,
                },
            ],
            clusters: vec![
                Cluster {
                    id: "ws/alpha".to_owned(),
                    member_ids: vec!["a".to_owned(), "b".to_owned()],
                    parent_breadcrumb: vec!["ws".to_owned()],
                    zoom_threshold: 1.0,
                },
                Cluster {
                    id: "ws/beta".to_owned(),
                    member_ids: vec!["c".to_owned()],
                    parent_breadcrumb: vec!["ws".to_owned()],
                    zoom_threshold: 1.0,
                },
            ],
        }
    }

    fn nav_view() -> GraphView {
        let mut v = view_with_graph(nav_graph());
        v.canvas = CanvasRect {
            ox: 0.0,
            oy: 0.0,
            w: 800.0,
            h: 600.0,
        };
        v
    }

    #[test]
    fn enter_cluster_arms_tween_and_scopes() {
        let mut v = nav_view();
        let base = Instant::now();
        v.enter_cluster_by_id("ws/alpha", base);

        assert!(v.navigator.is_scoped());
        assert!(v.camera_transition.is_some(), "zoom-in tween armed");
        let m = v.navigator.members().unwrap();
        assert!(m.contains("a") && m.contains("b") && !m.contains("c"));

        // Completing the tween lands the camera on the cluster-fit target.
        let step = v.advance_camera_tween(base + Duration::from_millis(450));
        assert!(matches!(step, CameraStep::Completed));
        assert!(v.camera_transition.is_none());
        let bounds =
            navigation::camera::members_bounds(&["a".to_owned(), "b".to_owned()], &v.layout, 0.0)
                .unwrap();
        let expect = navigation::camera::camera_to_fit(
            bounds,
            800.0,
            600.0,
            v.navigator.config.cluster_fit_margin,
        );
        assert_eq!(v.camera, expect);
    }

    #[test]
    fn leave_scope_tweens_back_to_saved_camera() {
        let mut v = nav_view();
        let base = Instant::now();
        let saved = Camera {
            pan_x: 12.0,
            pan_y: 5.0,
            zoom: 0.9,
        };
        v.camera = saved;
        v.enter_cluster_by_id("ws/alpha", base);
        let _ = v.advance_camera_tween(base + Duration::from_millis(450));

        v.leave_scope(base + Duration::from_millis(500));
        assert!(!v.navigator.is_scoped());
        let step = v.advance_camera_tween(base + Duration::from_millis(950));
        assert!(matches!(step, CameraStep::Completed));
        assert_eq!(v.camera, saved, "zoom-out restores the pre-enter camera");
    }

    #[test]
    fn exit_edge_jump_chains_leave_then_enter() {
        let mut v = nav_view();
        let base = Instant::now();
        v.enter_cluster_by_id("ws/alpha", base);
        let _ = v.advance_camera_tween(base + Duration::from_millis(450));

        // The exit-chip click path: queue the target, leave.
        v.pending_enter = Some("ws/beta".to_owned());
        v.leave_scope(base + Duration::from_millis(500));

        // Phase 1 completes → the pending enter arms phase 2 (still Running).
        let step = v.advance_camera_tween(base + Duration::from_millis(950));
        assert!(matches!(step, CameraStep::Running), "chained zoom-in armed");
        assert!(v.pending_enter.is_none());
        assert_eq!(
            v.navigator.scope().map(|s| s.cluster_id.as_str()),
            Some("ws/beta")
        );

        // Phase 2 completes on the beta fit.
        let step = v.advance_camera_tween(base + Duration::from_millis(1400));
        assert!(matches!(step, CameraStep::Completed));
        assert!(v.camera_transition.is_none());
    }

    #[test]
    fn scoped_render_output_filters_and_appends_exit_stubs() {
        let mut v = nav_view();
        let base = Instant::now();
        let rect = v.canvas;
        let full = v.render_output(rect, v.camera).unwrap();
        assert_eq!(full.spheres.len(), 3);

        v.enter_cluster_by_id("ws/alpha", base);
        let _ = v.advance_camera_tween(base + Duration::from_millis(450));
        let scoped = v.render_output(rect, v.camera).unwrap();
        assert_eq!(scoped.spheres.len(), 2, "only alpha members draw");
        // a→b internal (1 solid) + a→c exit stub (fade segments).
        assert_eq!(
            scoped.edges.len(),
            1 + v.navigator.config.exit_stub_segments,
            "internal edge + faded exit stub"
        );
        // The hidden node is no longer hit-testable.
        let c_pos = v.layout.get("c").unwrap();
        let vp = v.viewport(rect);
        let (cx_, cy_) = vp.model_to_screen(c_pos);
        assert_ne!(scoped.hit_test(cx_, cy_), Some("c"));
    }

    // ── Auto-clustering (Slice C-cluster) ────────────────────────────────

    /// 12 nodes in two 6-member clusters; config trips auto-clustering.
    fn cluster_view_graph() -> GraphView {
        let mut g = WorkspaceGraph::default();
        for c in ["alpha", "beta"] {
            let mut members = Vec::new();
            for i in 0..6 {
                let id = format!("{c}-n{i}");
                g.nodes.push(node(&id, &format!("ws/{c}/{id}.rs")));
                members.push(id);
            }
            g.clusters.push(Cluster {
                id: format!("ws/{c}"),
                member_ids: members,
                parent_breadcrumb: vec!["ws".to_owned()],
                zoom_threshold: 2.0,
            });
        }
        g.edges.push(Edge {
            src: "alpha-n0".to_owned(),
            dst: "beta-n0".to_owned(),
            rel_type: RelType::Calls,
            weight: 1.0,
        });
        let mut v = view_with_graph(g);
        v.canvas = CanvasRect {
            ox: 0.0,
            oy: 0.0,
            w: 800.0,
            h: 600.0,
        };
        v.cluster_view.config = cluster::ClusterConfig {
            auto_threshold_nodes: 10,
            target_visible_nodes: 2,
            min_fold_size: 3,
            boundary_pad_px: 18.0,
        };
        let zoom = v.camera.zoom;
        let graph = v.graph.clone();
        v.cluster_view.rebuild(&graph, zoom);
        v
    }

    #[test]
    fn folded_view_renders_cluster_spheres_and_rerouted_edge() {
        let v = cluster_view_graph();
        assert!(v.cluster_view.is_active());
        assert_eq!(v.cluster_view.folded_count(), 2);

        let (dg, _) = v.display_graph_layout();
        assert_eq!(dg.nodes.len(), 2, "two cluster spheres only");
        assert!(dg.nodes.iter().all(|n| n.id.starts_with("cluster::")));
        assert_eq!(dg.edges.len(), 1, "cross edge re-routed sphere→sphere");

        // The render output draws the display graph; cluster spheres are
        // hit-testable by their synthetic ids.
        let out = v.render_output(v.canvas, v.camera).unwrap();
        assert_eq!(out.spheres.len(), 2);
        assert!(out.spheres.iter().all(|s| s.id.starts_with("cluster::")));
    }

    #[test]
    fn scoped_view_bypasses_clustering() {
        let mut v = cluster_view_graph();
        v.enter_cluster_by_id("ws/alpha", Instant::now());
        let (dg, _) = v.display_graph_layout();
        assert_eq!(
            dg.nodes.len(),
            12,
            "scoped path uses the real graph (scope filter applies at render)"
        );
        let out = v.render_output(v.canvas, v.camera).unwrap();
        assert_eq!(out.spheres.len(), 6, "alpha members, no spheres folded");
    }

    #[test]
    fn toggle_expand_then_boundary_after_anim() {
        let mut v = cluster_view_graph();
        // Manual expand override (the menu's action without the gpui menu).
        v.cluster_view
            .set_override("ws/alpha", cluster::Override::Expanded);
        let t0 = Instant::now();
        let anim = (300.0, layout::CubicBezier::new(0.16, 1.0, 0.3, 1.0));
        assert!(v.cluster_view.sync(v.camera.zoom, t0, anim));
        v.cluster_view.advance(t0 + Duration::from_millis(400));

        let (dg, _) = v.display_graph_layout();
        assert!(
            dg.nodes.iter().any(|n| n.id == "alpha-n0"),
            "alpha expanded in place"
        );
        assert!(
            dg.nodes.iter().any(|n| n.id == "cluster::ws/beta"),
            "beta still folded"
        );
        // The expanded-in-place boundary outline lands in the frame.
        let out = v.render_output(v.canvas, v.camera).unwrap();
        assert_eq!(out.outlines.len(), 1);
        assert!(out.outlines[0].w > 0.0 && out.outlines[0].h > 0.0);
    }

    #[test]
    fn graph_reload_resets_scope() {
        let mut v = nav_view();
        v.enter_cluster_by_id("ws/alpha", Instant::now());
        assert!(v.navigator.is_scoped());
        // Reload path resets navigation (mirrors spawn_load's Ok branch).
        v.navigator.reset();
        v.camera_transition = None;
        v.pending_enter = None;
        assert!(!v.navigator.is_scoped() && v.navigator.members().is_none());
    }
}
