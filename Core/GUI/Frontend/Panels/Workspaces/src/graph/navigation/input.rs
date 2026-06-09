//! Pointer + keyboard input handlers for the graph canvas.
//!
//! Moved verbatim out of `graph/mod.rs` (2026-06-09 pre-C-navigation
//! cleanup); no behaviour change. C-navigation extends these handlers with
//! zoom-toward-cursor, cluster enter/leave thresholds, breadcrumb clicks and
//! exit-edge clicks (translated into `NavAction`s for the navigator).

use gpui::{
    Context, KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta,
    ScrollWheelEvent, Window,
};

use super::super::GraphView;

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
    pub(crate) fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.key.as_str() == "l" && ks.modifiers.control && ks.modifiers.shift {
            let next = self.current_layout.next();
            self.set_layout(next, cx);
        }
    }

    pub(crate) fn on_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
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

    pub(crate) fn on_down(&mut self, ev: &MouseDownEvent, _cx: &mut Context<Self>) {
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
