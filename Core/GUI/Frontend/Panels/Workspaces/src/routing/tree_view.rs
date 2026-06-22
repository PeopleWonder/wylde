//! The concept-routing **R3b** dependency-tree view — a read-only gpui canvas
//! that draws the workspace's typed relation DAG as a tree, mounted beside the
//! Relations editor (concept-routing plan §5, relation-model addendum §4.3).
//!
//! ## What it shows
//!
//! The pure [`super::tree`] model projects the relation overview into the
//! **shipped graph render stack**: the `Hierarchical` layout backend lays
//! depends-on edges out as a tidy tree (parents above children), and the
//! `RenderOutput` draw list is painted by the shipped `graph::paint::paint_graph`.
//! Edge kinds are visually distinct (dependency = solid bright arrow, exclusion
//! = dashed red severed cut, positive = light link) and node labels overlay the
//! spheres.
//!
//! ## Interaction (read-only)
//!
//! Pan (drag empty space) + zoom (scroll). **Clicking a node re-centres** the
//! camera on it AND emits [`TreeEvent::Selected`] — the host
//! ([`super::RelationsView`]) consumes that to deep-link its editor onto the
//! node (reusing the editor's `set_focus`), so the tree and editor stay in step.
//! Nothing here writes the relation store.

use gpui::{
    canvas, div, prelude::*, px, App, Bounds, Context, ElementId, EventEmitter, FocusHandle,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Window,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::graph::model::Layout;
use crate::graph::paint::{overlay_text, paint_graph};
use crate::graph::render::{Camera, Viewport};

use super::ipc::{self, NodeRefView};
use super::tree::{self, TreeModel};

/// Event the tree emits on a node click so the host can deep-link the editor.
#[derive(Clone, Debug)]
pub enum TreeEvent {
    /// The user clicked a node — re-centre the editor on it.
    Selected(NodeRefView),
}

/// The canvas rect (window-absolute px) captured at paint time so mouse
/// handlers can project model↔screen for hit-testing.
#[derive(Clone, Copy, Debug, Default)]
struct CanvasRect {
    ox: f32,
    oy: f32,
    w: f32,
    h: f32,
}

/// A pan-drag in progress.
#[derive(Clone, Copy, Debug)]
struct PanDrag {
    x: f32,
    y: f32,
    moved: bool,
}

/// Below this total cursor travel a press counts as a click (re-centre), not a
/// pan.
const CLICK_SLOP: f32 = 4.0;

/// The dependency-tree sub-view.
pub struct DependencyTreeView {
    workspace_id: Option<String>,
    model: TreeModel,
    layout: Layout,
    camera: Camera,
    canvas: CanvasRect,
    dark: bool,
    fitted: bool,
    loading: bool,
    error: Option<String>,
    drag: Option<PanDrag>,
    focus: Option<FocusHandle>,
}

impl EventEmitter<TreeEvent> for DependencyTreeView {}

impl DependencyTreeView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let view = Self {
            workspace_id: None,
            model: TreeModel::default(),
            layout: Layout::default(),
            camera: Camera::default(),
            canvas: CanvasRect::default(),
            dark: true,
            fitted: false,
            loading: true,
            error: None,
            drag: None,
            focus: None,
        };
        Self::spawn_load(cx);
        view
    }

    /// Resolve the active workspace, load the relation graph + node universe,
    /// and build the tree model (the `reducer::overview` shape feeds it).
    pub fn spawn_load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws = crate::vocabulary::ipc::active_workspace().await;
            let (ws_id, model, error) = match ws {
                Ok(Some(id)) => {
                    let universe = ipc::load_node_universe(&id).await;
                    match ipc::load_graph(&id).await {
                        Ok(rels) => {
                            let rows = super::reducer::overview(&rels);
                            (Some(id), tree::build_tree(&rows, &universe), None)
                        }
                        Err(e) => (Some(id), TreeModel::default(), Some(e)),
                    }
                }
                Ok(None) => (None, TreeModel::default(), None),
                Err(e) => (None, TreeModel::default(), Some(e)),
            };
            let _ = this.update(app_cx, |v, cx| {
                let layout = model.layout();
                v.loading = false;
                v.workspace_id = ws_id;
                v.model = model;
                v.layout = layout;
                v.fitted = false; // re-fit the camera to the freshly built tree
                v.error = error;
                cx.notify();
            });
        })
        .detach();
    }

    /// Reload (the Refresh control / host on tab focus).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        Self::spawn_load(cx);
        cx.notify();
    }

    // ── observability accessors (windowed tests) ─────────────────────────

    pub fn is_loading(&self) -> bool {
        self.loading
    }
    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }
    pub fn node_count(&self) -> usize {
        self.model.nodes.len()
    }
    pub fn edge_count(&self) -> usize {
        self.model.edges.len()
    }

    fn viewport(&self, rect: CanvasRect, camera: Camera) -> Viewport {
        Viewport {
            origin_x: rect.ox,
            origin_y: rect.oy,
            width: rect.w,
            height: rect.h,
            camera,
            dark: self.dark,
        }
    }

    /// Centre the camera on a node (re-centre on click), keeping the zoom.
    fn center_on(&mut self, token: &str) {
        if let Some(pos) = self.layout.get(token) {
            // model_to_screen puts the model origin at canvas centre + pan; to
            // centre `pos`, pan must cancel its scaled offset.
            self.camera.pan_x = -pos.x * self.camera.zoom;
            self.camera.pan_y = -pos.y * self.camera.zoom;
        }
    }

    // ── mouse ─────────────────────────────────────────────────────────────

    fn on_down(&mut self, ev: &MouseDownEvent, _cx: &mut Context<Self>) {
        let (sx, sy) = (f32::from(ev.position.x), f32::from(ev.position.y));
        self.drag = Some(PanDrag {
            x: sx,
            y: sy,
            moved: false,
        });
    }

    fn on_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let (px_, py) = (f32::from(ev.position.x), f32::from(ev.position.y));
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let (dx, dy) = (px_ - drag.x, py - drag.y);
        if !drag.moved && dx.abs() + dy.abs() < CLICK_SLOP {
            return;
        }
        if let Some(d) = self.drag.as_mut() {
            d.moved = true;
            d.x = px_;
            d.y = py;
        }
        self.camera.pan_x += dx;
        self.camera.pan_y += dy;
        cx.notify();
    }

    fn on_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.moved {
            return; // a pan, not a click
        }
        // A click: hit-test the tree, re-centre on the node, and emit the
        // deep-link selection for the host editor.
        let (sx, sy) = (f32::from(ev.position.x), f32::from(ev.position.y));
        let vp = self.viewport(self.canvas, self.camera);
        let out = tree::render_tree(&self.model, &self.layout, &vp, self.dark);
        if let Some(token) = out.hit_test(sx, sy).map(str::to_owned) {
            if let Some(node) = self.model.node_for_token(&token).cloned() {
                self.center_on(&token);
                cx.emit(TreeEvent::Selected(node));
                cx.notify();
            }
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let units = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
        };
        if units.abs() < f32::EPSILON {
            return;
        }
        self.camera.zoom_by(1.1f32.powf(units));
        cx.notify();
    }

    /// The paint canvas: capture bounds, fit once, build the draw list, paint.
    fn canvas_element(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let model = self.model.clone();
        let layout = self.layout.clone();
        let camera = self.camera;
        let dark = self.dark;
        let fitted = self.fitted;

        canvas(
            move |bounds: Bounds<Pixels>, _window, app: &mut App| -> Option<crate::graph::render::RenderOutput> {
                let rect = CanvasRect {
                    ox: f32::from(bounds.origin.x),
                    oy: f32::from(bounds.origin.y),
                    w: f32::from(bounds.size.width),
                    h: f32::from(bounds.size.height),
                };
                // One-time fit on the first non-empty paint.
                let mut cam = camera;
                if !fitted && !model.nodes.is_empty() && rect.w > 0.0 {
                    if let Some(bb) = model.graph.model_bounds(&layout) {
                        cam.zoom = Viewport::fit_zoom(bb, rect.w, rect.h);
                    }
                }
                entity.update(app, |view, _| {
                    view.canvas = rect;
                    if !view.fitted && cam != view.camera {
                        view.camera = cam;
                    }
                    view.fitted = true;
                });
                let vp = Viewport {
                    origin_x: rect.ox,
                    origin_y: rect.oy,
                    width: rect.w,
                    height: rect.h,
                    camera: cam,
                    dark,
                };
                Some(tree::render_tree(&model, &layout, &vp, dark))
            },
            move |_bounds, output, window, _app| {
                if let Some(out) = output {
                    paint_graph(window, &out);
                }
            },
        )
        .absolute()
        .size_full()
    }

    /// Node-label chips, absolutely positioned over the canvas at each node's
    /// projected centre. Uses the last-captured canvas rect (one frame behind on
    /// the very first paint, like the graph panel's exit chips).
    fn label_chips(
        &self,
        mut content: gpui::Stateful<gpui::Div>,
        _cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        if self.canvas.w <= 0.0 {
            return content;
        }
        let vp = self.viewport(self.canvas, self.camera);
        for (i, n) in self.model.nodes.iter().take(120).enumerate() {
            let Some(pos) = self.layout.get(&n.token) else {
                continue;
            };
            let (sx, sy) = vp.model_to_screen(pos);
            content = content.child(
                div()
                    .id(("tree-label", i))
                    .absolute()
                    .left(px(sx - self.canvas.ox + 8.0))
                    .top(px(sy - self.canvas.oy - 6.0))
                    .text_size(px(size::MICRO))
                    .text_color(gpui::rgb(0xD0D4DA))
                    .child(SharedString::from(n.label.clone())),
            );
        }
        content
    }
}

impl Render for DependencyTreeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let bg = gpui::Rgba {
            r: 0.05,
            g: 0.06,
            b: 0.09,
            a: 1.0,
        };

        let content_id: ElementId = ElementId::Name("routing-tree-canvas".into());
        let mut content = div()
            .id(content_id)
            .track_focus(&focus)
            .relative()
            .w_full()
            .h(px(420.0))
            .overflow_hidden()
            .bg(bg)
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| this.on_scroll(ev, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| this.on_down(ev, cx)),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| this.on_move(ev, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _w, cx| this.on_up(ev, cx)),
            );

        if !self.model.nodes.is_empty() {
            content = content.child(self.canvas_element(cx));
            content = self.label_chips(content, cx);
        }

        // Status overlay (top-left): a short legend + state.
        let legend = if self.loading {
            "Loading dependency tree…".to_owned()
        } else if let Some(err) = &self.error {
            format!("Relation store unreachable — {err}")
        } else if self.model.nodes.is_empty() {
            "No relations yet — author dependencies/exclusions in the editor to see the tree."
                .to_owned()
        } else {
            format!(
                "Dependency tree · {} node(s) · {} edge(s) · → depends-on · ⊘ dashed-red = IS NOT · click a node to focus the editor",
                self.model.nodes.len(),
                self.model.edges.len()
            )
        };
        content = content.child(
            div()
                .absolute()
                .top_2()
                .left_2()
                .font_family(FAMILY_INTER)
                .child(overlay_text(legend, size::MICRO, weight::REGULAR)),
        );

        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<DependencyTreeView>();
    }
}
