//! `Viewport` + `Camera` — the screen window onto model space.
//!
//! C-scaffold carries a **basic** camera (pan + zoom) so the data → screen
//! path is interactive; the space-map navigation (scroll-zoom into clusters,
//! breadcrumb, exit edges) is Slice C-navigation. Viewport culling
//! ([`Viewport::visible_model_rect`]) is a placeholder here — it returns the
//! whole visible region; real level-of-detail / culling is C-navigation /
//! render::lod (a later file).
//!
//! `model_to_screen` is the one projection both the renderer and hit-testing
//! go through, so a 3D renderer swap only rewrites this function.

use crate::graph::model::Position;

/// Pan/zoom state, owned by the graph view and Copy so handlers mutate a
/// local copy then store it back. `pan` is a **screen-space** offset in px
/// (added after the model→screen scale); `zoom` is the model→px scale factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            pan_x: 0.0,
            pan_y: 0.0,
            zoom: 1.0,
        }
    }
}

/// Hard zoom limits so a scroll-storm can't invert or explode the scene.
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 8.0;

impl Camera {
    /// Multiply the zoom by `factor`, clamped. Pan is unchanged (C-navigation
    /// adds zoom-toward-cursor); this keeps the scaffold camera trivial.
    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    }

    /// Drag-pan by a screen-space delta in px.
    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan_x += dx;
        self.pan_y += dy;
    }
}

/// A frame's view onto the scene: the canvas rect (window-absolute px), the
/// camera, and the active light/dark mode.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    /// Canvas top-left in window coordinates (px).
    pub origin_x: f32,
    pub origin_y: f32,
    pub width: f32,
    pub height: f32,
    pub camera: Camera,
    pub dark: bool,
}

impl Viewport {
    /// Project a model-space position to **window** pixel coordinates (the
    /// same space mouse events arrive in, so hit-testing reuses this). The
    /// model origin maps to the canvas centre; `z` is ignored in 2D.
    pub fn model_to_screen(&self, p: Position) -> (f32, f32) {
        let cx = self.origin_x + self.width * 0.5;
        let cy = self.origin_y + self.height * 0.5;
        (
            cx + self.camera.pan_x + p.x * self.camera.zoom,
            cy + self.camera.pan_y + p.y * self.camera.zoom,
        )
    }

    /// Inverse of [`model_to_screen`](Self::model_to_screen): window pixel →
    /// model space (`z` recovered as 0). Used to translate a drag cursor into
    /// the model position the physics worker should pin a node to.
    pub fn screen_to_model(&self, sx: f32, sy: f32) -> Position {
        let cx = self.origin_x + self.width * 0.5;
        let cy = self.origin_y + self.height * 0.5;
        let zoom = if self.camera.zoom.abs() < f32::EPSILON {
            1.0
        } else {
            self.camera.zoom
        };
        Position {
            x: (sx - cx - self.camera.pan_x) / zoom,
            y: (sy - cy - self.camera.pan_y) / zoom,
            z: 0.0,
        }
    }

    /// Pick the zoom that fits a model bounding box into the canvas with a
    /// margin, clamped. Used on first load so the whole graph is visible.
    /// `(min_x, min_y, max_x, max_y)`.
    pub fn fit_zoom(bounds: (f32, f32, f32, f32), width: f32, height: f32) -> f32 {
        let span_x = (bounds.2 - bounds.0).max(1.0);
        let span_y = (bounds.3 - bounds.1).max(1.0);
        // 0.85 leaves a comfortable margin around the cloud.
        let zx = (width * 0.85) / span_x;
        let zy = (height * 0.85) / span_y;
        zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// The model-space rect currently on screen as `(min_x, min_y, max_x,
    /// max_y)` — the inverse-projected canvas box. The renderer (G2) culls
    /// nodes/edges against this so per-frame cost tracks the *visible* count,
    /// not the total.
    pub fn visible_model_rect(&self) -> (f32, f32, f32, f32) {
        let inv = if self.camera.zoom.abs() < f32::EPSILON {
            1.0
        } else {
            1.0 / self.camera.zoom
        };
        let half_w = self.width * 0.5 * inv;
        let half_h = self.height * 0.5 * inv;
        let mx = -self.camera.pan_x * inv;
        let my = -self.camera.pan_y * inv;
        (mx - half_w, my - half_h, mx + half_w, my + half_h)
    }

    /// [`visible_model_rect`](Self::visible_model_rect) grown by `margin_frac`
    /// of its half-extent on every side. Culling uses the grown rect so a node
    /// just past the edge (or an edge whose endpoint is) is kept — avoids
    /// pop-in while panning, at the cost of drawing a thin border ring of
    /// off-screen geometry.
    pub fn visible_model_rect_expanded(&self, margin_frac: f32) -> (f32, f32, f32, f32) {
        let (min_x, min_y, max_x, max_y) = self.visible_model_rect();
        let mx = (max_x - min_x) * 0.5 * margin_frac;
        let my = (max_y - min_y) * 0.5 * margin_frac;
        (min_x - mx, min_y - my, max_x + mx, max_y + my)
    }
}

/// Whether two axis-aligned rects (`min_x, min_y, max_x, max_y`) overlap.
/// Edge culling uses it: an edge's endpoint-bbox that doesn't intersect the
/// visible rect means the whole segment is off-screen and can be skipped —
/// and because a segment that *crosses* the rect always has a bbox that
/// overlaps it, this never drops a visible edge (only a cheap superset is
/// kept). Used by `render_2d`.
#[inline]
pub fn rects_overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 <= b.2 && a.2 >= b.0 && a.1 <= b.3 && a.3 >= b.1
}

/// Whether a point lies inside a rect (`min_x, min_y, max_x, max_y`).
#[inline]
pub fn rect_contains(r: (f32, f32, f32, f32), x: f32, y: f32) -> bool {
    x >= r.0 && x <= r.2 && y >= r.1 && y <= r.3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(zoom: f32) -> Viewport {
        Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 200.0,
            height: 100.0,
            camera: Camera {
                pan_x: 0.0,
                pan_y: 0.0,
                zoom,
            },
            dark: true,
        }
    }

    #[test]
    fn model_origin_maps_to_canvas_centre() {
        let (x, y) = vp(1.0).model_to_screen(Position::default());
        assert_eq!((x, y), (100.0, 50.0));
    }

    #[test]
    fn zoom_scales_distance_from_centre() {
        let p = Position {
            x: 10.0,
            y: 0.0,
            z: 0.0,
        };
        let (x1, _) = vp(1.0).model_to_screen(p);
        let (x2, _) = vp(2.0).model_to_screen(p);
        assert_eq!(x1 - 100.0, 10.0);
        assert_eq!(x2 - 100.0, 20.0);
    }

    #[test]
    fn pan_offsets_in_screen_space() {
        let mut v = vp(1.0);
        v.camera.pan_by(15.0, -5.0);
        let (x, y) = v.model_to_screen(Position::default());
        assert_eq!((x, y), (115.0, 45.0));
    }

    #[test]
    fn zoom_by_is_clamped() {
        let mut c = Camera::default();
        c.zoom_by(1000.0);
        assert_eq!(c.zoom, MAX_ZOOM);
        c.zoom_by(0.0001);
        assert_eq!(c.zoom, MIN_ZOOM);
    }

    #[test]
    fn screen_to_model_inverts_model_to_screen() {
        let mut v = vp(1.7);
        v.camera.pan_by(23.0, -11.0);
        let p = Position {
            x: 42.0,
            y: -8.0,
            z: 0.0,
        };
        let (sx, sy) = v.model_to_screen(p);
        let back = v.screen_to_model(sx, sy);
        assert!((back.x - p.x).abs() < 1e-3 && (back.y - p.y).abs() < 1e-3);
        assert_eq!(back.z, 0.0);
    }

    #[test]
    fn expanded_rect_grows_symmetrically() {
        let v = vp(1.0); // 200×100, no pan/zoom → rect (-100,-50,100,50)
        let base = v.visible_model_rect();
        assert_eq!(base, (-100.0, -50.0, 100.0, 50.0));
        // +20% of half-extent: x half-extent 100 → +20 each side, y 50 → +10.
        let grown = v.visible_model_rect_expanded(0.2);
        assert!((grown.0 - -120.0).abs() < 1e-3);
        assert!((grown.1 - -60.0).abs() < 1e-3);
        assert!((grown.2 - 120.0).abs() < 1e-3);
        assert!((grown.3 - 60.0).abs() < 1e-3);
    }

    #[test]
    fn rect_overlap_and_contains() {
        let r = (0.0, 0.0, 10.0, 10.0);
        assert!(rect_contains(r, 5.0, 5.0));
        assert!(!rect_contains(r, 11.0, 5.0));
        // Overlapping rects.
        assert!(rects_overlap(r, (5.0, 5.0, 20.0, 20.0)));
        // Touching at an edge counts as overlap (inclusive bounds).
        assert!(rects_overlap(r, (10.0, 0.0, 12.0, 5.0)));
        // Fully disjoint.
        assert!(!rects_overlap(r, (11.0, 11.0, 20.0, 20.0)));
        // A segment crossing the rect has a bbox that overlaps it — the
        // property edge-culling relies on (no visible edge dropped).
        let crossing_bbox = (-5.0, 5.0, 15.0, 5.0); // horizontal line through r
        assert!(rects_overlap(crossing_bbox, r));
    }

    #[test]
    fn fit_zoom_keeps_within_limits() {
        let z = Viewport::fit_zoom((-1000.0, -1000.0, 1000.0, 1000.0), 200.0, 100.0);
        assert!((MIN_ZOOM..=MAX_ZOOM).contains(&z));
        // Tiny graph → clamped to MAX, not infinite.
        let z2 = Viewport::fit_zoom((0.0, 0.0, 0.0, 0.0), 200.0, 100.0);
        assert_eq!(z2, MAX_ZOOM);
    }
}
