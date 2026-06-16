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
pub mod outline_view;
mod paint;
pub mod physics;
pub mod render;
pub mod settings;
mod transition_driver;
pub mod vocabulary;

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
use settings::{persistence, GraphProfile, ProfileLibrary, DEFAULT_PROFILE};
use transition_driver::ActiveTransition;

/// Lifecycle name of the workspaces backend — target of the graph view's
/// Start/Restart recovery affordance (decision 7).
const GRAPH_WORKSPACES_SERVICE: &str = "wylde-workspaces";

/// Which one-click recovery a recoverable graph error offers (decision 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphRecovery {
    /// Workspaces service is down → start it (`service.start`).
    StartService,
    /// Workspaces service is out of date → restart it (`service.restart`).
    RestartService,
    /// Graph DB (Bolt/Memgraph) is down → start it (`start_graph_database`).
    StartGraphDb,
}

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
    /// The global profile library + per-workspace bookmarks (C-settings),
    /// loaded from `<data_dir>/graph_profiles.json` at mount.
    profiles: ProfileLibrary,
    /// Where the library persists. Resolved once at mount; tests point it at
    /// a temp file so profile operations never touch the real data dir.
    profiles_path: std::path::PathBuf,
    /// Last persistence/parse error, surfaced in the Settings tab.
    profiles_error: Option<String>,
    /// Name of the profile currently applied.
    active_profile: String,
    /// Whether the breadcrumb-bar quick-switcher dropdown is open.
    profile_menu_open: bool,
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
    /// The selected node — set by a click (opens its file outline) or
    /// programmatically by [`GraphView::focus_node`] (the S6 deep-link entry
    /// point); surfaced in the header.
    last_clicked: Option<String>,
    /// Which graph layer is showing (Slice N): `V` cycles CodeGraph →
    /// Overlay → VocabularyGraph. Render-only — see `display_graph_layout`.
    view_mode: model::ViewMode,
    /// The per-file outline side card (Slice H): opened by clicking a node
    /// with a source file; fed by `treesitter.outline`.
    outline: Option<outline_view::OutlineState>,
    /// The saved anchors, fetched alongside the graph and resolved against
    /// it (Slice N stage N-3). Empty until the first anchor load (or when
    /// both stores are unreachable — the overlay just draws nothing extra).
    vocab_anchors: Vec<vocabulary::projection::ResolvedAnchor>,
}

impl GraphView {
    pub fn new() -> Self {
        let (theme, theme_error) = match Theme::load_v1() {
            Ok(t) => (Some(Rc::new(t)), None),
            Err(e) => (None, Some(e)),
        };
        let profiles_path = persistence::profiles_path();
        let (profiles, profiles_error) = persistence::load_from(&profiles_path);
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
            profiles,
            profiles_path,
            profiles_error,
            active_profile: DEFAULT_PROFILE.to_owned(),
            profile_menu_open: false,
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
            view_mode: model::ViewMode::default(),
            outline: None,
            vocab_anchors: Vec::new(),
        }
    }

    /// Create the view entity and kick off the initial graph load.
    pub fn new_entity(cx: &mut App) -> gpui::Entity<Self> {
        cx.new(|cx| {
            let view = Self::new();
            Self::spawn_load(cx);
            Self::spawn_theme_hot_reload(cx);
            view
        })
    }

    /// Dev theme hot-reload (debug builds + `WYLDE_THEME_PATH` set): poll
    /// the on-disk Visual Style YAML's mtime every 500 ms and re-parse +
    /// re-apply the Theme on change — colour/easing/size tweaks repaint
    /// live, zero rebuild. A no-op (the loop never spawns) in release
    /// builds or without the env var, so the shipped path is untouched.
    fn spawn_theme_hot_reload(cx: &mut Context<Self>) {
        let Some(initial_mtime) = render::style::dev_theme_mtime() else {
            return;
        };
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let mut last = initial_mtime;
            loop {
                app_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let Some(mtime) = render::style::dev_theme_mtime() else {
                    continue; // file briefly missing mid-save — keep polling
                };
                if mtime == last {
                    continue;
                }
                last = mtime;
                // `load_v1` prefers the on-disk YAML in this mode and falls
                // back to embedded on a mid-edit parse error.
                let parsed = Theme::load_v1();
                let alive = this
                    .update(app_cx, |view, cx| {
                        match parsed {
                            Ok(t) => {
                                view.theme = Some(Rc::new(t));
                                view.theme_error = None;
                                eprintln!("[theme-hot] re-applied visual style");
                            }
                            Err(e) => view.theme_error = Some(e),
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
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
                        // C-settings: the workspace's bookmarked profile
                        // applies first (knobs + layout); the session
                        // layout_cache only fills in when no bookmark exists.
                        let pointer_profile = view
                            .workspace_id
                            .as_ref()
                            .and_then(|ws| view.profiles.pointer(ws))
                            .map(str::to_owned)
                            .and_then(|name| view.profiles.get(&name).cloned());
                        let kind = match &pointer_profile {
                            Some(p) => {
                                view.active_profile = p.name.clone();
                                view.dark = p.theme.dark;
                                view.navigator.config = p.interaction.navigation;
                                view.cluster_view.config = p.graph.cluster;
                                p.graph.layout_kind()
                            }
                            None => {
                                view.active_profile = DEFAULT_PROFILE.to_owned();
                                view.workspace_id
                                    .as_ref()
                                    .and_then(|id| view.layout_cache.get(id).copied())
                                    .unwrap_or_default()
                            }
                        };
                        // Re-run the one-time cluster assignment + auto-fold
                        // selection (under the profile's knobs); the snap to
                        // the post-fit zoom happens at the first paint (see
                        // canvas_element).
                        view.cluster_view.rebuild(&view.graph, view.camera.zoom);
                        view.cluster_menu = None;
                        view.profile_menu_open = false;
                        // A reload may invalidate the outlined file (Slice H).
                        view.outline = None;
                        // Deterministic layouts compute their final positions
                        // and leave the physics worker paused; force-directed
                        // warm-starts (depth-banded) and spins up the worker
                        // to refine off-thread.
                        view.current_layout = kind;
                        view.layout = Rc::new(kind.compute_positions(view.graph.as_ref()));
                        // Re-fit the camera to the freshly loaded graph.
                        view.fitted = false;
                        if kind.is_physics() {
                            view.start_physics(cx);
                        } else {
                            view.physics = None;
                        }
                        // Fetch + resolve the vocabulary anchors against the
                        // fresh graph (Slice N overlay). Stale resolutions
                        // from the old graph are dropped immediately.
                        view.vocab_anchors.clear();
                        Self::spawn_anchor_load(cx);
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

    /// One-click recovery for a recoverable graph error (decision 7): drive the
    /// reusable lifecycle helper for `action`, then reload the graph so a
    /// now-reachable backend repopulates without a manual retry. A control
    /// failure surfaces as a logical error on the banner.
    fn spawn_recovery(&self, action: GraphRecovery, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = match action {
                GraphRecovery::StartService => {
                    wylde_gui_pipe::start_service(GRAPH_WORKSPACES_SERVICE).await
                }
                GraphRecovery::RestartService => {
                    wylde_gui_pipe::restart_service(GRAPH_WORKSPACES_SERVICE).await
                }
                GraphRecovery::StartGraphDb => wylde_gui_pipe::start_graph_database().await,
            };
            let _ = this.update(app_cx, |view, cx| match outcome {
                Ok(_) => GraphView::spawn_load(cx),
                Err(e) => {
                    view.error = Some(ipc::GraphFetchError::Logical(e));
                    cx.notify();
                }
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
    /// The vocabulary overlay (Slice N) applies after clustering, against
    /// the *display* layout — an anchor whose symbol is folded away
    /// gracefully falls back to the edge spiral, and its tether edge is
    /// skipped by the renderer (missing endpoint).
    fn display_graph_layout(&self) -> (Rc<WorkspaceGraph>, Rc<Layout>) {
        let (base_g, base_l) = if !self.navigator.is_scoped() {
            match self.cluster_view.apply(&self.graph, &self.layout) {
                Some((g, l)) => (Rc::new(g), Rc::new(l)),
                None => (self.graph.clone(), self.layout.clone()),
            }
        } else {
            (self.graph.clone(), self.layout.clone())
        };
        if self.view_mode != model::ViewMode::CodeGraph && !self.vocab_anchors.is_empty() {
            let proj = vocabulary::projection::project(&self.vocab_anchors, &base_l);
            if let Some((g, l)) =
                vocabulary::overlay::apply(self.view_mode, &base_g, &base_l, &proj)
            {
                return (Rc::new(g), Rc::new(l));
            }
        }
        (base_g, base_l)
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
            mode: self.view_mode,
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
        // Profile quick-switcher dropdown (C-settings).
        if let Some(menu) = self.profile_menu_element(cx) {
            content = content.child(menu);
        }
        // Per-file outline side card (Slice H).
        if let Some(card) = self.outline_element(cx) {
            content = content.child(card);
        }
        content = content.child(self.overlay(cx));

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

    // ── Settings profiles (Slice C-settings) ────────────────────────────

    /// Apply a named profile: re-point every knob struct, swap the layout
    /// (animated) when it differs, run the Theme `graph_profile_switch`
    /// camera tween into the re-fitted view, bookmark it for the active
    /// workspace, and persist. `false` when no such profile exists.
    pub(crate) fn apply_profile(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.profiles.get(name).cloned() else {
            return false;
        };
        self.active_profile = p.name.clone();
        self.profile_menu_open = false;
        self.dark = p.theme.dark;
        self.navigator.config = p.interaction.navigation;
        self.cluster_view.config = p.graph.cluster;
        // Re-select auto-folds under the new clustering knobs.
        let graph = self.graph.clone();
        self.cluster_view.rebuild(&graph, self.camera.zoom);

        let kind = p.graph.layout_kind();
        if kind != self.current_layout {
            self.set_layout(kind, cx);
        }

        // 500 ms camera tween into the new view (whole-graph re-fit).
        if self.canvas.w > 0.0 && !self.navigator.is_scoped() {
            if let Some(bb) = self.graph.model_bounds(&self.layout) {
                let target = navigation::camera::camera_to_fit(
                    bb,
                    self.canvas.w,
                    self.canvas.h,
                    0.85, // the first-load fit margin (Viewport::fit_zoom)
                );
                self.begin_camera_tween(target, "graph_profile_switch", Instant::now());
                self.spawn_camera_driver(cx);
            }
        }

        if let Some(ws) = self.workspace_id.clone() {
            self.profiles.set_pointer(&ws, name);
        }
        self.persist_profiles();
        cx.notify();
        true
    }

    /// Deep-link focus (S6, plan P1.4): the programmatic entry point that
    /// drives the graph to a specific node — used by cross-panel deep-links
    /// (S7: click a vocab word → open the graph on that symbol) and, later,
    /// jumps from the editor. Selects the node, reveals it if it sits in a
    /// folded cluster, tweens the camera to centre on it (keeping the user's
    /// zoom), and opens its file outline — the same surface a click produces.
    /// No-op when the node isn't in the loaded graph.
    ///
    /// Returns `true` when the node was found and focused.
    pub fn focus_node(&mut self, node_id: &str, cx: &mut Context<Self>) -> bool {
        if !self.graph.nodes.iter().any(|n| n.id == node_id) {
            return false;
        }
        // Reveal a folded cluster containing the node so it's actually visible.
        if let Some(cluster) = self.cluster_view.cluster_of(node_id).map(str::to_owned) {
            if self.cluster_view.is_folded(&cluster) {
                self.toggle_cluster_fold(&cluster, true, cx);
            }
        }
        // Centre the camera on the node (positions live in the base physics
        // layout — present even while the node is folded into a cluster).
        if let Some(pos) = self.layout.get(node_id) {
            let zoom = self.camera.zoom;
            let target = navigation::camera::camera_to_center(pos.x, pos.y, zoom);
            self.begin_camera_tween(target, "graph_profile_switch", Instant::now());
            self.spawn_camera_driver(cx);
        }
        // Mirror a click: select + open the file outline when the node has one.
        let file = self
            .graph
            .nodes
            .iter()
            .find(|n| n.id == node_id)
            .map(|n| n.file.clone())
            .filter(|f| !f.is_empty());
        self.last_clicked = Some(node_id.to_owned());
        if let Some(file) = file {
            self.open_outline(file, cx);
        }
        cx.notify();
        true
    }

    /// Snapshot the live knobs as a (new or replaced) named profile, make it
    /// active, bookmark it, and persist. Errors on blank names.
    pub(crate) fn save_current_profile(&mut self, name: &str) -> Result<(), String> {
        let name = name.trim();
        let profile = GraphProfile::capture(
            name,
            self.current_layout,
            self.cluster_view.config,
            self.navigator.config,
            self.dark,
        );
        if !self.profiles.upsert(profile) {
            return Err("profile name cannot be empty".to_owned());
        }
        self.active_profile = name.to_owned();
        if let Some(ws) = self.workspace_id.clone() {
            self.profiles.set_pointer(&ws, name);
        }
        self.persist_profiles();
        match &self.profiles_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Delete a profile (the default is permanent). If it was active, fall
    /// back to the default profile's knobs.
    pub(crate) fn delete_profile(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        if !self.profiles.remove(name) {
            return false;
        }
        if self.active_profile == name {
            self.apply_profile(DEFAULT_PROFILE, cx);
        } else {
            self.persist_profiles();
            cx.notify();
        }
        true
    }

    /// Write the library to `<data_dir>/graph_profiles.json`; failures are
    /// stashed for the Settings tab (the panel keeps working in-memory).
    fn persist_profiles(&mut self) {
        self.profiles_error = persistence::save_to(&self.profiles_path, &self.profiles).err();
    }

    // ── Settings-tab accessors / knob setters (C-settings) ──────────────

    pub(crate) fn profile_names(&self) -> Vec<String> {
        self.profiles
            .names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    pub(crate) fn active_profile_name(&self) -> &str {
        &self.active_profile
    }

    pub(crate) fn profiles_error(&self) -> Option<&str> {
        self.profiles_error.as_deref()
    }

    pub(crate) fn nav_config(&self) -> navigation::NavConfig {
        self.navigator.config
    }

    pub(crate) fn cluster_config(&self) -> cluster::ClusterConfig {
        self.cluster_view.config
    }

    pub(crate) fn dark_mode(&self) -> bool {
        self.dark
    }

    pub(crate) fn current_layout_kind(&self) -> LayoutKind {
        self.current_layout
    }

    /// Live-update the navigation knobs (Settings tab "Apply").
    pub(crate) fn set_nav_config(&mut self, c: navigation::NavConfig, cx: &mut Context<Self>) {
        self.navigator.config = c;
        cx.notify();
    }

    /// Live-update the clustering knobs: re-select auto-folds and re-sync.
    pub(crate) fn set_cluster_config(&mut self, c: cluster::ClusterConfig, cx: &mut Context<Self>) {
        self.cluster_view.config = c;
        let graph = self.graph.clone();
        self.cluster_view.rebuild(&graph, self.camera.zoom);
        self.sync_clusters(cx);
        cx.notify();
    }

    pub(crate) fn set_dark_mode(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        cx.notify();
    }

    /// Switch layout from the Settings tab (same animated swap as
    /// `Ctrl+Shift+L`).
    pub(crate) fn choose_layout(&mut self, kind: LayoutKind, cx: &mut Context<Self>) {
        self.set_layout(kind, cx);
    }

    /// The breadcrumb-bar quick-switcher dropdown (Theme
    /// `ui_chrome.context_menu`): one row per profile; click applies with the
    /// `graph_profile_switch` tween.
    fn profile_menu_element(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        if !self.profile_menu_open {
            return None;
        }
        let theme = self.theme.as_ref()?;
        let m = &theme.ui_chrome.context_menu;
        let mut menu = div()
            .id("graph-profile-menu")
            .absolute()
            .top_1()
            .right_2()
            .bg(to_rgba(m.background(self.dark)))
            .rounded(px(m.border_radius_px))
            .overflow_hidden()
            .text_size(px(m.font_size_px))
            .text_color(to_rgba(theme.graph_panel.breadcrumb_bar.text(self.dark)))
            .flex()
            .flex_col();
        for (i, name) in self.profile_names().into_iter().enumerate() {
            let marker = if name == self.active_profile {
                "● "
            } else {
                "  "
            };
            let target = name.clone();
            menu = menu.child(
                div()
                    .id(("graph-profile-menu-item", i))
                    .h(px(m.item_height_px))
                    .px(px(m.item_padding_px))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            this.apply_profile(&target, cx);
                        }),
                    )
                    .child(SharedString::from(format!("{marker}{name}"))),
            );
        }
        Some(menu)
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
        let mode = self.view_mode;
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
                    mode,
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
    fn overlay(&self, cx: &mut Context<Self>) -> gpui::Div {
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
            "Graph · {title} · {} nodes · {} edges · zoom {:.0}% · {layout} · view {}",
            self.graph.nodes.len(),
            self.graph.edges.len(),
            self.camera.zoom * 100.0,
            self.view_mode_label()
        );
        col = col.child(overlay_text(status, font_size::XS, weight::SEMIBOLD));
        let hint = if self.navigator.is_scoped() {
            "Scroll — zoom · Esc — zoom out · Ctrl+Shift+L — cycle layout · V — vocabulary"
        } else {
            "Scroll — zoom into clusters · Ctrl+Shift+L — cycle layout · V — vocabulary"
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
            // Graceful degrade banner (OI-1 / F2). Two recoverable states get a
            // click-to-retry chip, but with the RIGHT words: "down" → Start;
            // "out of date" (no_action: running binary lacks the verb) → Update.
            // Telling the user to start an already-running service was the bug.
            if err.is_recoverable() {
                // Three recoverable states, each with the RIGHT one-click fix
                // (decision 7): graph-db down → Start graph database; service
                // out of date → Restart; service down → Start. Plus a real
                // click-to-retry below. (Telling the user to start an
                // already-running service was the F2 bug.)
                let (banner, label, action) = if err.is_graph_db_down() {
                    (
                        "Graph database isn't running — the code graph is stored in Memgraph \
                         (Bolt :7687).",
                        "Start graph database",
                        GraphRecovery::StartGraphDb,
                    )
                } else if err.is_out_of_date() {
                    (
                        "Workspaces service is out of date — its build doesn't have the \
                         code-graph yet.",
                        "Restart service",
                        GraphRecovery::RestartService,
                    )
                } else {
                    (
                        "Workspaces service isn't running — showing last-known graph.",
                        "Start service",
                        GraphRecovery::StartService,
                    )
                };
                col = col.child(overlay_text(banner.to_owned(), font_size::XS, weight::REGULAR));
                // The one-click recovery button.
                col = col.child(
                    overlay_text(label.to_owned(), font_size::XS, weight::SEMIBOLD)
                        .id("workspaces-graph-recover")
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut GraphView, _ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.spawn_recovery(action, cx);
                            }),
                        ),
                );
                // Retry once the underlying fix has landed.
                col = col.child(
                    overlay_text("Click to retry".to_owned(), font_size::MICRO, weight::REGULAR)
                        .id("workspaces-graph-retry")
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                GraphView::spawn_load(cx);
                            }),
                        ),
                );
            } else {
                col = col.child(overlay_text(
                    err.message().to_owned(),
                    font_size::XS,
                    weight::REGULAR,
                ));
            }
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

    // ── Settings profiles (Slice C-settings) ─────────────────────────────

    #[test]
    fn save_current_profile_captures_bookmarks_and_persists() {
        let dir = std::env::temp_dir()
            .join("wylde-graphview-profile-tests")
            .join(format!("save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut v = nav_view(); // workspace_id = "demo"
        v.profiles_path = dir.join("graph_profiles.json");
        v.navigator.config.zoom_step_factor = 1.3;
        v.current_layout = LayoutKind::Hierarchical;
        v.dark = false;

        v.save_current_profile("Focus").expect("saves clean");
        assert_eq!(v.active_profile, "Focus");
        let p = v.profiles.get("Focus").unwrap();
        assert_eq!(p.graph.layout_kind(), LayoutKind::Hierarchical);
        assert!((p.interaction.navigation.zoom_step_factor - 1.3).abs() < 1e-6);
        assert!(!p.theme.dark);
        assert_eq!(
            v.profiles.pointer("demo"),
            Some("Focus"),
            "active workspace bookmarked"
        );

        // The library round-trips off disk, default profile intact.
        let (lib, err) = settings::persistence::load_from(&v.profiles_path);
        assert!(err.is_none());
        assert!(lib.get("Focus").is_some());
        assert!(lib.get(DEFAULT_PROFILE).is_some());
        assert_eq!(lib.pointer("demo"), Some("Focus"));

        // Blank names are rejected and change nothing.
        assert!(v.save_current_profile("   ").is_err());
        assert_eq!(v.active_profile, "Focus");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_view_always_offers_the_default_profile() {
        let v = GraphView::new();
        assert!(v.profile_names().iter().any(|n| n == DEFAULT_PROFILE));
        assert_eq!(v.active_profile_name(), DEFAULT_PROFILE);
        assert!(!v.profile_menu_open);
    }

    // ── Vocabulary overlay (Slice N stage N-3) ───────────────────────────

    use vocabulary::projection::{anchor_node_id, resolve, AnchorSpec};

    fn anchored_view() -> GraphView {
        let mut v = view_with_graph(swap_graph()); // nodes a, b, c
        let specs = vec![
            AnchorSpec {
                identifier: "tether_a".to_owned(),
                target_symbol: Some("a".to_owned()),
                related_to: vec!["idea".to_owned()],
            },
            AnchorSpec {
                identifier: "idea".to_owned(),
                target_symbol: None,
                related_to: vec![],
            },
        ];
        v.vocab_anchors = resolve(&specs, &v.graph);
        v
    }

    #[test]
    fn code_graph_mode_ignores_anchors() {
        let v = anchored_view();
        assert_eq!(v.view_mode, model::ViewMode::CodeGraph);
        let (dg, _) = v.display_graph_layout();
        assert_eq!(dg.nodes.len(), 3, "no anchor nodes in code mode");
    }

    #[test]
    fn overlay_mode_appends_anchor_nodes_and_positions() {
        let mut v = anchored_view();
        v.view_mode = model::ViewMode::Overlay;
        let (dg, dl) = v.display_graph_layout();
        assert_eq!(dg.nodes.len(), 3 + 2, "code nodes + both anchors");
        assert!(dg.nodes.iter().any(|n| n.id == anchor_node_id("tether_a")));
        assert!(dg
            .nodes
            .iter()
            .filter(|n| n.id.starts_with(vocabulary::projection::ANCHOR_NODE_PREFIX))
            .all(|n| n.kind == model::NodeKind::Anchor));
        assert!(dg.nodes.iter().all(|n| dl.get(&n.id).is_some()));
        // The anchors render: spheres for every node incl. anchors.
        let rect = CanvasRect {
            ox: 0.0,
            oy: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let out = v.render_output(rect, Camera::default()).unwrap();
        assert_eq!(out.spheres.len(), 5);
    }

    #[test]
    fn vocabulary_mode_draws_the_anchor_world_alone() {
        let mut v = anchored_view();
        v.view_mode = model::ViewMode::VocabularyGraph;
        let (dg, _) = v.display_graph_layout();
        assert_eq!(dg.nodes.len(), 2, "anchors only");
        assert!(dg
            .nodes
            .iter()
            .all(|n| n.id.starts_with(vocabulary::projection::ANCHOR_NODE_PREFIX)));
        // Peer edge survives; the tether to code node "a" is dropped.
        assert_eq!(dg.edges.len(), 1);
        assert_eq!(dg.edges[0].rel_type, model::RelType::RelatedTo);
    }

    #[test]
    fn empty_vocabulary_keeps_the_base_in_any_mode() {
        let mut v = view_with_graph(swap_graph());
        v.view_mode = model::ViewMode::Overlay;
        let (dg, _) = v.display_graph_layout();
        assert_eq!(dg.nodes.len(), 3, "nothing to project — base graph drawn");
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

    // ── Windowed graph deep-link (focus_node) ────────────────────────────
    //
    // The terminal of the cross-panel focus bus (S7): a vocab word in the
    // InferenceBar pushes a `WorkspaceFocus { node_id }`; the Workspaces panel
    // drains it and calls `GraphView::focus_node`. The bus push/drain has its
    // own unit test (`focus_bus`), and the panel's tab routing is covered in
    // `tests/registry_nav.rs`; this pins the GraphView end — that a present
    // node is found + centred (selected as last-clicked) and an absent one is
    // a clean no-op. Driven in a real window because focus_node tweens the
    // camera + opens the file outline (both spawn async effects).

    use gpui::TestAppContext;
    use wylde_gui_test_support::ScriptedBackend;

    #[gpui::test]
    fn focus_node_centres_a_present_node_and_ignores_absent(cx: &mut TestAppContext) {
        // Absorb the outline IPC focus_node fires for a node with a file.
        let _guard = ScriptedBackend::new().install();

        let g = WorkspaceGraph {
            nodes: vec![node("sym::foo", "src/foo.rs"), node("sym::bar", "src/bar.rs")],
            edges: vec![],
            clusters: vec![],
        };
        let window = cx.add_window(|_w, _cx| view_with_graph(g));
        cx.run_until_parked();

        // A present deep-link target is focused (true) and recorded as the
        // selected node — the same surface a click produces.
        let hit = window
            .update(cx, |gv, _w, cx| gv.focus_node("sym::foo", cx))
            .unwrap();
        assert!(hit, "a present node is focused — the vocab-word deep-link terminal");
        cx.run_until_parked();
        window
            .update(cx, |gv, _w, _cx| {
                assert_eq!(
                    gv.last_clicked.as_deref(),
                    Some("sym::foo"),
                    "focusing selects the node (mirrors a click)"
                );
            })
            .unwrap();

        // An absent target is a no-op (false), leaving the prior selection.
        let miss = window
            .update(cx, |gv, _w, cx| gv.focus_node("sym::missing", cx))
            .unwrap();
        assert!(!miss, "a deep-link to a node not in the loaded graph is a no-op");
        window
            .update(cx, |gv, _w, _cx| {
                assert_eq!(gv.last_clicked.as_deref(), Some("sym::foo"), "selection unchanged");
            })
            .unwrap();
    }
}
