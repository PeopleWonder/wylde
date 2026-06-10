//! Scope-aware camera math (Slice C-navigation) — pure functions over the
//! projection primitive in [`render::viewport::Camera`], which stays the one
//! model↔screen seam (no render API break).
//!
//! Everything here is gpui-free and unit-tested directly: zoom-toward-cursor
//! (the model point under the pointer stays put while the scale changes),
//! fit-a-bounds-to-the-canvas (used to frame an entered cluster), and
//! member-set bounding boxes (a cluster's extent in model space).
//!
//! [`render::viewport::Camera`]: crate::graph::render::viewport::Camera

use crate::graph::model::Layout;
use crate::graph::render::viewport::{Camera, Viewport, MAX_ZOOM, MIN_ZOOM};

/// Multiply the camera zoom by `factor`, keeping the model point under the
/// cursor (`sx`, `sy`, window px) fixed on screen — the "zoom toward the
/// cursor" feel of every map UI.
///
/// Derivation: the projection is `s = centre + pan + p·zoom`, so holding `s`
/// and `centre` fixed across `zoom → zoom'` requires
/// `pan' = pan + p·(zoom − zoom')`.
pub fn zoom_toward(cam: &mut Camera, factor: f32, sx: f32, sy: f32, vp: &Viewport) {
    let p = vp.screen_to_model(sx, sy);
    let z0 = cam.zoom;
    let z1 = (z0 * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    cam.pan_x += p.x * (z0 - z1);
    cam.pan_y += p.y * (z0 - z1);
    cam.zoom = z1;
}

/// The camera that frames `bounds` (model space, `(min_x, min_y, max_x,
/// max_y)`) in a `width × height` canvas with `margin` (fraction of the
/// canvas the bounds fill). Zoom clamped to the hard camera limits; pan
/// centres the bounds on the canvas centre.
pub fn camera_to_fit(bounds: (f32, f32, f32, f32), width: f32, height: f32, margin: f32) -> Camera {
    let span_x = (bounds.2 - bounds.0).max(1.0);
    let span_y = (bounds.3 - bounds.1).max(1.0);
    let zx = (width * margin) / span_x;
    let zy = (height * margin) / span_y;
    let zoom = zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM);
    let cx = (bounds.0 + bounds.2) * 0.5;
    let cy = (bounds.1 + bounds.3) * 0.5;
    Camera {
        pan_x: -cx * zoom,
        pan_y: -cy * zoom,
        zoom,
    }
}

/// Bounding box of `member_ids`' positions in model space, or `None` when no
/// member has a position. `pad` (model units) expands the box on every side
/// so a cluster's "extent" includes some breathing room around node centres.
pub fn members_bounds(
    member_ids: &[String],
    layout: &Layout,
    pad: f32,
) -> Option<(f32, f32, f32, f32)> {
    let mut bb: Option<(f32, f32, f32, f32)> = None;
    for id in member_ids {
        let Some(p) = layout.get(id) else { continue };
        bb = Some(match bb {
            None => (p.x, p.y, p.x, p.y),
            Some(b) => (b.0.min(p.x), b.1.min(p.y), b.2.max(p.x), b.3.max(p.y)),
        });
    }
    bb.map(|b| (b.0 - pad, b.1 - pad, b.2 + pad, b.3 + pad))
}

/// Is the model-space point `(x, y)` inside `bounds`?
pub fn bounds_contain(bounds: (f32, f32, f32, f32), x: f32, y: f32) -> bool {
    x >= bounds.0 && x <= bounds.2 && y >= bounds.1 && y <= bounds.3
}

/// Bounds area (for smallest-wins disambiguation when overlapping clusters
/// both cross their threshold under the cursor).
pub fn bounds_area(bounds: (f32, f32, f32, f32)) -> f32 {
    (bounds.2 - bounds.0).max(0.0) * (bounds.3 - bounds.1).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::Position;
    use std::collections::HashMap;

    fn vp(cam: Camera) -> Viewport {
        Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800.0,
            height: 600.0,
            camera: cam,
            dark: true,
        }
    }

    fn layout(items: &[(&str, f32, f32)]) -> Layout {
        let map: HashMap<String, Position> = items
            .iter()
            .map(|(id, x, y)| {
                (
                    (*id).to_owned(),
                    Position {
                        x: *x,
                        y: *y,
                        z: 0.0,
                    },
                )
            })
            .collect();
        Layout::from_positions(map)
    }

    #[test]
    fn zoom_toward_keeps_cursor_model_point_fixed() {
        let mut cam = Camera {
            pan_x: 13.0,
            pan_y: -7.0,
            zoom: 1.0,
        };
        let v = vp(cam);
        let (sx, sy) = (615.0, 120.0); // arbitrary off-centre cursor
        let before = v.screen_to_model(sx, sy);

        zoom_toward(&mut cam, 1.6, sx, sy, &v);

        let after = vp(cam).screen_to_model(sx, sy);
        assert!((before.x - after.x).abs() < 1e-3, "{before:?} vs {after:?}");
        assert!((before.y - after.y).abs() < 1e-3);
        assert!((cam.zoom - 1.6).abs() < 1e-6);
    }

    #[test]
    fn zoom_toward_clamps_at_limits_without_drift() {
        let mut cam = Camera::default();
        let v = vp(cam);
        // Slam past MAX: zoom clamps; the invariant holds for the clamped zoom.
        zoom_toward(&mut cam, 1e6, 100.0, 100.0, &v);
        assert_eq!(cam.zoom, MAX_ZOOM);
        // At the clamp, further zooming is a no-op (pan untouched).
        let pan = (cam.pan_x, cam.pan_y);
        let v2 = vp(cam);
        zoom_toward(&mut cam, 2.0, 100.0, 100.0, &v2);
        assert_eq!(cam.zoom, MAX_ZOOM);
        assert_eq!((cam.pan_x, cam.pan_y), pan);
    }

    #[test]
    fn zoom_toward_centre_is_pure_zoom() {
        // Cursor on the canvas centre with zero pan → model origin → pan stays 0.
        let mut cam = Camera::default();
        let v = vp(cam);
        zoom_toward(&mut cam, 2.0, 400.0, 300.0, &v);
        assert!(cam.pan_x.abs() < 1e-4 && cam.pan_y.abs() < 1e-4);
        assert_eq!(cam.zoom, 2.0);
    }

    #[test]
    fn camera_to_fit_centres_and_fills_with_margin() {
        // A 100×50 box at (100..200, 100..150) into an 800×600 canvas @ 0.85.
        let cam = camera_to_fit((100.0, 100.0, 200.0, 150.0), 800.0, 600.0, 0.85);
        // Limiting axis is x: zoom = 800·0.85 / 100 = 6.8.
        assert!((cam.zoom - 6.8).abs() < 1e-3);
        // The box centre (150, 125) projects to the canvas centre.
        let v = vp(cam);
        let (sx, sy) = v.model_to_screen(Position {
            x: 150.0,
            y: 125.0,
            z: 0.0,
        });
        assert!((sx - 400.0).abs() < 1e-2 && (sy - 300.0).abs() < 1e-2);
    }

    #[test]
    fn camera_to_fit_clamps_zoom() {
        let tiny = camera_to_fit((0.0, 0.0, 0.1, 0.1), 800.0, 600.0, 0.85);
        assert!(tiny.zoom <= MAX_ZOOM);
        let huge = camera_to_fit((-1e6, -1e6, 1e6, 1e6), 800.0, 600.0, 0.85);
        assert!(huge.zoom >= MIN_ZOOM);
    }

    #[test]
    fn members_bounds_covers_members_and_pads() {
        let l = layout(&[
            ("a", 0.0, 0.0),
            ("b", 10.0, 20.0),
            ("ghost-skipped", 0.0, 0.0),
        ]);
        let ids = vec!["a".to_owned(), "b".to_owned(), "missing".to_owned()];
        let bb = members_bounds(&ids, &l, 5.0).unwrap();
        assert_eq!(bb, (-5.0, -5.0, 15.0, 25.0));
        assert!(bounds_contain(bb, 10.0, 20.0));
        assert!(!bounds_contain(bb, 50.0, 0.0));
        assert!(bounds_area(bb) > 0.0);
    }

    #[test]
    fn members_bounds_none_when_no_member_placed() {
        let l = layout(&[("x", 0.0, 0.0)]);
        assert!(members_bounds(&["nope".to_owned()], &l, 1.0).is_none());
        assert!(members_bounds(&[], &l, 1.0).is_none());
    }
}
