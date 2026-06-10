//! gpui paint helpers — translate a renderer [`RenderOutput`] into draw calls.
//!
//! Pure free functions split out of `graph/mod.rs` (2026-06-09 pre-C-navigation
//! cleanup); no behaviour change. Everything here is geometry/colour plumbing:
//! the *what to draw* lives in `render/` (Theme-driven), the *how gpui paints
//! it* lives here.

use gpui::{div, point, prelude::*, px, size, Bounds, Path, Pixels, Window};
use gpui::{FontWeight, SharedString};
use wylde_theme::typography::FAMILY_INTER;

use super::render::{Color, RenderOutput};
use crate::workspaces_panel::pack;

/// One line of overlay text.
pub(super) fn overlay_text(s: String, sz: f32, w: u16) -> gpui::Div {
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
pub(super) fn paint_graph(window: &mut Window, out: &RenderOutput) {
    // Boundary outlines first (under everything).
    for r in &out.outlines {
        window.paint_quad(gpui::quad(
            Bounds {
                origin: point(px(r.x), px(r.y)),
                size: size(px(r.w), px(r.h)),
            },
            r.corner_radius,
            to_rgba(r.fill),
            px(r.border_width),
            to_hsla(r.border),
            gpui::BorderStyle::Solid,
        ));
    }
    // Edges next (under the spheres).
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
pub(super) fn to_rgba(c: Color) -> gpui::Rgba {
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
