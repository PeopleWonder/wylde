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
pub mod layout;
pub mod model;
pub mod physics;
pub mod render;

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
use std::time::Duration;
use std::time::Instant;

use gpui::{
    canvas, div, point, prelude::*, px, size, App, AppContext, AsyncApp, Bounds, Context,
    ElementId, FocusHandle, FontWeight, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Path, Pixels, Render, ScrollDelta, ScrollWheelEvent,
    SharedString, Window,
};
use wylde_theme::typography::{size as font_size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;
use ipc::{GraphFetchError, GraphLoad};
use layout::{CubicBezier, ForceDirected, LayoutKind, LayoutTransition};
use model::{Layout, WorkspaceGraph};
use physics::{ActiveRegion, PhysicsConfig, PhysicsHandle, PositionFrame};
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

/// In-flight drag state. A press that lands on a node drags that node (pins it
/// in the physics worker); a press on empty space pans the camera.
#[derive(Clone, Debug)]
struct Drag {
    x: f32,
    y: f32,
    /// Set once the pointer moves past the click/drag threshold, so a release
    /// without movement is treated as a click (node hit-test) instead of a pan.
    moved: bool,
    /// `Some(id)` → dragging that node (pin it to the cursor each move);
    /// `None` → panning the camera.
    node: Option<String>,
}

/// An in-flight animated layout swap (Slice C-layout). The pure tween lives in
/// [`LayoutTransition`]; this pairs it with a wall-clock start and the target
/// layout so the driver can finalise (resume / leave-paused physics) on
/// completion.
struct ActiveTransition {
    anim: LayoutTransition,
    start: Instant,
    target: LayoutKind,
}

/// Result of advancing the layout-swap tween one step.
enum TransitionStep {
    Running,
    Completed,
}

/// Tween tick cadence (~60 fps). The animation runs on the gpui main thread,
/// independent of the physics worker's own frame interval.
const TRANSITION_FRAME: Duration = Duration::from_millis(16);

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
            physics: None,
            current_layout: LayoutKind::default(),
            layout_cache: HashMap::new(),
            transition: None,
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
                        // A reload cancels any in-flight swap from the old graph.
                        view.transition = None;
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

    // ── Layout swap (Slice C-layout) ─────────────────────────────────────

    /// Switch to `kind` with the locked 500 ms animated tween (Visual Style v1
    /// `graph_layout_swap`). No-op if already showing `kind`. Cycled by
    /// `Ctrl+Shift+L` via [`LayoutKind::next`].
    fn set_layout(&mut self, kind: LayoutKind, cx: &mut Context<Self>) {
        if !self.begin_layout_swap(kind, Instant::now()) {
            return;
        }
        self.spawn_transition_driver(cx);
        cx.notify();
    }

    /// Pure core of [`set_layout`]: snapshot the current positions as `from`,
    /// compute the target backend's positions as `to`, pause physics, and arm
    /// the tween. Returns `false` (no swap armed) when already on `kind`. `now`
    /// is injected so tests drive the animation deterministically.
    fn begin_layout_swap(&mut self, kind: LayoutKind, now: Instant) -> bool {
        if kind == self.current_layout && self.transition.is_none() {
            return false;
        }
        let from = (*self.layout).clone();
        let to = kind.compute_positions(self.graph.as_ref());
        // Pause physics for the swap. Deterministic targets never resume it;
        // force-directed resumes (seeded from `to`) when the tween completes.
        self.physics = None;
        let (duration_ms, easing) = self.swap_anim();
        self.transition = Some(ActiveTransition {
            anim: LayoutTransition::new(from, to, duration_ms, easing),
            start: now,
            target: kind,
        });
        self.current_layout = kind;
        if let Some(id) = &self.workspace_id {
            self.layout_cache.insert(id.clone(), kind);
        }
        true
    }

    /// Advance the in-flight tween to wall-clock `now`. Updates `self.layout`;
    /// on completion finalises — force-directed respawns the worker seeded from
    /// the target positions, deterministic layouts leave it paused. cx-free so
    /// tests step it directly; the gpui driver re-attaches the subscription.
    fn advance_transition(&mut self, now: Instant) -> TransitionStep {
        let (layout, done, target, final_to) = {
            let Some(t) = self.transition.as_ref() else {
                return TransitionStep::Completed;
            };
            let elapsed = now.saturating_duration_since(t.start).as_secs_f32() * 1000.0;
            if t.anim.is_done(elapsed) {
                (t.anim.to.clone(), true, t.target, Some(t.anim.to.clone()))
            } else {
                (t.anim.sample(elapsed), false, t.target, None)
            }
        };
        self.layout = Rc::new(layout);
        if !done {
            return TransitionStep::Running;
        }
        self.transition = None;
        self.physics = if target.is_physics() {
            self.spawn_worker(final_to.as_ref())
        } else {
            None
        };
        TransitionStep::Completed
    }

    /// Drive the tween on the gpui main thread: a ~60 fps timer feeds wall-clock
    /// into [`advance_transition`](Self::advance_transition) until it completes,
    /// then re-attaches the physics subscription (a no-op for a paused layout).
    fn spawn_transition_driver(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                app_cx.background_executor().timer(TRANSITION_FRAME).await;
                let running = this.update(app_cx, |view, cx| {
                    let step = view.advance_transition(Instant::now());
                    cx.notify();
                    matches!(step, TransitionStep::Running)
                });
                match running {
                    Ok(true) => continue,
                    Ok(false) => {
                        let _ = this.update(app_cx, |view, cx| view.subscribe_physics(cx));
                        break;
                    }
                    Err(_) => break, // the view is gone
                }
            }
        })
        .detach();
    }

    /// The layout-swap duration + easing, read FROM the theme
    /// (`animations.graph_layout_swap`); the fallback (used only when the theme
    /// failed to load) equals the locked spec value, so a swap still animates.
    fn swap_anim(&self) -> (f32, CubicBezier) {
        self.theme
            .as_ref()
            .and_then(|t| t.animation("graph_layout_swap"))
            .map(|a| (a.duration_ms, CubicBezier::from_array(a.easing)))
            .unwrap_or((500.0, CubicBezier::GRAPH_LAYOUT_SWAP))
    }

    /// `Ctrl+Shift+L` → cycle force → hierarchical → grid → force.
    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.key.as_str() == "l" && ks.modifiers.control && ks.modifiers.shift {
            let next = self.current_layout.next();
            self.set_layout(next, cx);
        }
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
        // A zoom is a resume trigger + changes which nodes are visible — refresh
        // the worker's cull region.
        self.push_viewport();
        cx.notify();
    }

    fn on_down(&mut self, ev: &MouseDownEvent, _cx: &mut Context<Self>) {
        let (sx, sy) = (f32::from(ev.position.x), f32::from(ev.position.y));
        // A press on a node drags that node; a press on empty space pans.
        let node = self
            .render_output(self.canvas, self.camera)
            .and_then(|out| out.hit_test(sx, sy).map(str::to_owned));
        self.drag = Some(Drag {
            x: sx,
            y: sy,
            moved: false,
            node,
        });
    }

    fn on_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let (px_, py) = (f32::from(ev.position.x), f32::from(ev.position.y));
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let (dx, dy) = (px_ - drag.x, py - drag.y);
        if !drag.moved && dx.abs() + dy.abs() < DRAG_THRESHOLD {
            return;
        }
        let node = drag.node.clone();
        if let Some(d) = self.drag.as_mut() {
            d.moved = true;
            d.x = px_;
            d.y = py;
        }
        match node {
            // Dragging a node: pin it to the cursor in model space; the worker
            // freezes its physics and the rest of the graph flows around it.
            Some(id) => {
                let m = self.viewport(self.canvas).screen_to_model(px_, py);
                if let Some(h) = &self.physics {
                    h.pin(id, m.x, m.y);
                }
            }
            // Empty space: pan the camera.
            None => self.camera.pan_by(dx, dy),
        }
        cx.notify();
    }

    fn on_up(&mut self, _ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        match drag.node {
            Some(id) => {
                if drag.moved {
                    // Drag finished — release the pin so the node rejoins the
                    // flow and settles into place.
                    if let Some(h) = &self.physics {
                        h.release(id);
                    }
                } else {
                    // A click (no movement) — record the selected node.
                    eprintln!("[workspaces.graph] clicked node {id}");
                    self.last_clicked = Some(id);
                    cx.notify();
                }
            }
            None => {
                if drag.moved {
                    // It was a pan — refresh the worker's cull region.
                    self.push_viewport();
                }
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

        // Focus handle (created lazily) so the canvas can capture
        // `Ctrl+Shift+L`. Clicking the canvas focuses it.
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();

        let root_id: ElementId = ElementId::Name("workspaces-graph-canvas".into());
        let mut root = div()
            .id(root_id)
            .track_focus(&focus)
            .size_full()
            .relative()
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
        col = col.child(overlay_text(
            "Ctrl+Shift+L — cycle layout".to_owned(),
            font_size::MICRO,
            weight::REGULAR,
        ));

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
}
