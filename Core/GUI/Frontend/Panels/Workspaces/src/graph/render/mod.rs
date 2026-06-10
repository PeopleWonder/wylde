//! Graph **render** — HOW the graph draws. **Pluggable 2D/3D** (Build Order
//! §8): the [`Renderer`] trait is the seam, [`render_2d::Renderer2d`] is the
//! v1 implementation, and a future `render_3d.rs` swaps in without touching
//! the model (`super::model`) or the (future) layout/physics modules.
//!
//! The renderer is **pure**: `frame` consumes a [`Scene`] (data + layout +
//! theme) and a [`Viewport`] (camera + canvas rect) and returns a
//! [`RenderOutput`] — a flat, gpui-free list of draw commands in **window
//! pixel** coordinates. The gpui hand-off (translating these into
//! `paint_quad` / `paint_path`) lives in the panel's canvas closure, so the
//! renderer is unit-testable with no window.

pub mod render_2d;
pub mod style;
pub mod viewport;

pub use style::{Color, Theme};
pub use viewport::{Camera, Viewport};

use super::model::{Layout, ViewMode, WorkspaceGraph};

/// Everything the renderer needs about *what* to draw — the data
/// (`super::model::workspace_graph` builds it), its computed positions, the
/// theme, and which layer is active. Borrowed so a frame allocates nothing
/// but its output.
pub struct Scene<'a> {
    pub graph: &'a WorkspaceGraph,
    pub layout: &'a Layout,
    pub theme: &'a Theme,
    pub mode: ViewMode,
    /// When the space-map is scoped into a cluster (C-navigation), the member
    /// ids to render. `None` → the whole graph. Edges with exactly one
    /// endpoint in scope are *not* drawn here — they become exit-edge fade
    /// stubs (computed by `navigation::compute_exit_edges`, appended by the
    /// view) so the boundary treatment stays a navigation concern.
    pub scope: Option<&'a std::collections::HashSet<String>>,
}

/// The pluggable rendering seam. v1: [`render_2d::Renderer2d`]; v2: a
/// `render_3d.rs` producing the same [`RenderOutput`] from projected 3D
/// positions.
pub trait Renderer {
    fn frame(&mut self, scene: &Scene<'_>, viewport: &Viewport) -> RenderOutput;
}

/// One concentric fill of a sphere, drawn back-to-front (rim → core →
/// specular). `dx`/`dy` offset the layer centre from the sphere centre (px) —
/// the specular highlight is offset toward the theme's `highlight_position`
/// to fake the radial gradient gpui can't draw natively at this rev.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SphereLayer {
    pub color: Color,
    pub dx: f32,
    pub dy: f32,
    pub radius: f32,
}

/// A drawable node: a stack of layers plus a thin border. `cx`/`cy`/`radius`
/// are the outer circle in window px (kept for hit-testing parity with the
/// panel's own test).
#[derive(Clone, Debug, PartialEq)]
pub struct SphereDraw {
    pub id: String,
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    pub layers: Vec<SphereLayer>,
    pub border_color: Color,
    pub border_width: f32,
}

/// A drawable line segment in window px. The renderer pre-segments dashed /
/// dotted edges into multiple solid `EdgeDraw`s, so the gpui paint layer only
/// ever draws solid lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeDraw {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub color: Color,
    pub thickness: f32,
}

/// A rounded-rect outline in window px (C-cluster: the Theme
/// `cluster_boundary` drawn around an expanded-in-place cluster's members).
/// Drawn under edges and spheres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutlineRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub corner_radius: f32,
    pub fill: Color,
    pub border: Color,
    pub border_width: f32,
}

/// A full frame's draw list. `bg_inner` / `bg_outer` are the radial-gradient
/// background endpoints (centre → edge); gpui lacks a radial gradient at this
/// rev so the panel approximates it (solid inner with a faint vignette).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderOutput {
    pub bg_inner: Color,
    pub bg_outer: Color,
    /// Boundary outlines (under everything else).
    pub outlines: Vec<OutlineRect>,
    pub edges: Vec<EdgeDraw>,
    pub spheres: Vec<SphereDraw>,
}

impl RenderOutput {
    /// Hit-test the topmost sphere whose disc contains `(x, y)` (window px).
    /// Iterates back-to-front so the last-drawn (visually on top) wins. Used
    /// by the panel's click handler to resolve a clicked node id.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&str> {
        self.spheres
            .iter()
            .rev()
            .find(|s| {
                let dx = x - s.cx;
                let dy = y - s.cy;
                dx * dx + dy * dy <= s.radius * s.radius
            })
            .map(|s| s.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sphere(id: &str, cx: f32, cy: f32, r: f32) -> SphereDraw {
        SphereDraw {
            id: id.to_owned(),
            cx,
            cy,
            radius: r,
            layers: vec![],
            border_color: Color::FALLBACK,
            border_width: 1.0,
        }
    }

    #[test]
    fn hit_test_finds_node_under_point() {
        let out = RenderOutput {
            spheres: vec![sphere("a", 10.0, 10.0, 5.0), sphere("b", 50.0, 50.0, 8.0)],
            ..Default::default()
        };
        assert_eq!(out.hit_test(11.0, 11.0), Some("a"));
        assert_eq!(out.hit_test(52.0, 48.0), Some("b"));
        assert_eq!(out.hit_test(200.0, 200.0), None, "empty space → no hit");
    }

    #[test]
    fn hit_test_prefers_topmost_on_overlap() {
        let out = RenderOutput {
            spheres: vec![
                sphere("under", 10.0, 10.0, 9.0),
                sphere("over", 12.0, 10.0, 9.0),
            ],
            ..Default::default()
        };
        // Both discs contain (11,10); the later-drawn "over" wins.
        assert_eq!(out.hit_test(11.0, 10.0), Some("over"));
    }
}
