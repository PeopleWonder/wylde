//! Pointer + keyboard input handlers for the graph canvas.
//!
//! C-navigation: scroll zooms **toward the cursor** (the model point under
//! the pointer stays put) and threshold crossings translate into
//! [`NavAction`]s — entering a cluster when the zoom crosses its
//! `zoom_threshold` under the cursor, leaving when it drops below the
//! scope's hysteresis point. `Esc` leaves the scope; breadcrumb / exit-chip
//! clicks are handled by their own elements (`breadcrumb.rs` /
//! `graph/mod.rs::exit_label_chips`).

use gpui::{
    Context, KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta,
    ScrollWheelEvent, Window,
};

use super::super::{ClusterMenu, GraphView};
use super::{camera, NavAction};
use crate::graph::cluster::cluster_id_from_node;

/// In-flight drag state. A press that lands on a node drags that node (pins it
/// in the physics worker); a press on empty space pans the camera.
#[derive(Clone, Debug)]
pub(crate) struct Drag {
    x: f32,
    y: f32,
    /// Set once the pointer moves past the click/drag threshold, so a release
    /// without movement is treated as a click (node hit-test) instead of a pan.
    moved: bool,
    /// `Some(id)` → dragging that node (pin it to the cursor each move);
    /// `None` → panning the camera.
    node: Option<String>,
}

/// Pointer movement (px) past which a press becomes a pan, not a click.
const DRAG_THRESHOLD: f32 = 3.0;

impl GraphView {
    /// `Ctrl+Shift+L` → cycle force → hierarchical → grid → force.
    /// `Esc` → leave the space-map scope (when scoped).
    pub(crate) fn on_key(
        &mut self,
        ev: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &ev.keystroke;
        if ks.key.as_str() == "l" && ks.modifiers.control && ks.modifiers.shift {
            let next = self.current_layout.next();
            self.set_layout(next, cx);
        }
        if ks.key.as_str() == "escape" {
            if self.cluster_menu.is_some() || self.profile_menu_open {
                // Esc closes open menus before it leaves a scope.
                self.cluster_menu = None;
                self.profile_menu_open = false;
                cx.notify();
            } else if self.navigator.is_scoped() {
                self.apply_nav_action(NavAction::LeaveScope, cx);
            }
        }
    }

    /// Right-click: offer Expand Cluster on a folded cluster sphere, Collapse
    /// Cluster on a member of an expandable (auto-fold) cluster.
    pub(crate) fn on_right_click(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let (sx, sy) = (f32::from(ev.position.x), f32::from(ev.position.y));
        let hit = self
            .render_output(self.canvas, self.camera)
            .and_then(|out| out.hit_test(sx, sy).map(str::to_owned));
        self.cluster_menu = None;
        if let Some(id) = hit {
            if let Some(cid) = cluster_id_from_node(&id) {
                self.cluster_menu = Some(ClusterMenu {
                    x: sx,
                    y: sy,
                    cluster_id: cid.to_owned(),
                    folded: true,
                });
            } else if !self.navigator.is_scoped() {
                if let Some(cid) = self.cluster_view.cluster_of(&id).map(str::to_owned) {
                    if self.cluster_view.is_expandable(&cid) {
                        self.cluster_menu = Some(ClusterMenu {
                            x: sx,
                            y: sy,
                            cluster_id: cid,
                            folded: false,
                        });
                    }
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn on_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let units = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
        };
        if units.abs() < f32::EPSILON {
            return;
        }
        // A manual scroll takes the camera back from any in-flight tween.
        self.camera_transition = None;
        self.pending_enter = None;

        let (sx, sy) = (f32::from(ev.position.x), f32::from(ev.position.y));
        let vp = self.viewport(self.canvas);
        let old_zoom = self.camera.zoom;
        // Zoom anchored at the cursor: the model point under the pointer is
        // invariant, so it's the same before and after the zoom.
        let cursor_model = vp.screen_to_model(sx, sy);
        camera::zoom_toward(
            &mut self.camera,
            self.navigator.config.zoom_step_factor.powf(units),
            sx,
            sy,
            &vp,
        );

        // Threshold crossings enter/leave the space-map scope.
        if let Some(action) = self.navigator.action_for_zoom(
            old_zoom,
            self.camera.zoom,
            (cursor_model.x, cursor_model.y),
            &self.graph,
            &self.layout,
        ) {
            self.apply_nav_action(action, cx);
        }

        // Clusters fold/unfold as the zoom crosses their thresholds.
        self.sync_clusters(cx);

        // A zoom is a resume trigger + changes which nodes are visible — refresh
        // the worker's cull region.
        self.push_viewport();
        cx.notify();
    }

    pub(crate) fn on_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        // A left press anywhere dismisses open menus (their items' own
        // handlers stop propagation before this runs).
        if self.cluster_menu.take().is_some() || self.profile_menu_open {
            self.profile_menu_open = false;
            cx.notify();
        }
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

    pub(crate) fn on_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
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

    pub(crate) fn on_up(&mut self, _ev: &MouseUpEvent, cx: &mut Context<Self>) {
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
