//! Animated **layout swap** — the pure tween that morphs node positions from
//! one layout to another over a fixed duration (Visual Style v1
//! `animations.graph_layout_swap`: 500 ms, easing `[0.77, 0, 0.175, 1]`).
//!
//! This module is **gpui-free and fully unit-testable**: a [`LayoutTransition`]
//! holds the start/end [`Layout`]s, the duration, and the easing curve, and
//! [`sample`](LayoutTransition::sample) returns the interpolated layout at a
//! given elapsed time. The GraphView drives it from the gpui main thread (a
//! frame timer feeds wall-clock elapsed into `sample`); the physics worker is
//! paused for the duration so the animation owns positions while it runs.
//!
//! Why here and not `navigation/transition.rs` (Build Order's file slot): the
//! navigation module is C-navigation's territory (camera zoom tweens) and does
//! not exist yet. The *layout*-swap tween belongs with the layouts it morphs
//! between, so it lives in `layout/`. C-navigation's `transition.rs` will host
//! the camera tweens; this stays the layout-swap tween. (Spec file-tree nuance,
//! not a behavioural divergence.)

use crate::graph::model::{Layout, Position};

/// A CSS-style cubic-bézier easing curve `(x1, y1, x2, y2)` with implicit
/// endpoints `(0,0)` and `(1,1)`. `ease(t)` maps linear progress `t ∈ [0,1]`
/// to eased progress by solving the curve for the bézier parameter at `x = t`
/// then returning `y`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Build from the theme's 4-element easing array `[x1, y1, x2, y2]`.
    pub fn from_array(a: [f32; 4]) -> Self {
        Self::new(a[0], a[1], a[2], a[3])
    }

    /// The locked `graph_layout_swap` easing (Visual Style v1) — used only as a
    /// degrade fallback when the theme failed to load (it equals the spec
    /// value, so a missing theme still animates correctly).
    pub const GRAPH_LAYOUT_SWAP: CubicBezier = CubicBezier::new(0.77, 0.0, 0.175, 1.0);

    /// Bézier component value at parameter `u` for control points `(0, p1, p2,
    /// 1)` — the standard cubic with fixed endpoints.
    fn comp(p1: f32, p2: f32, u: f32) -> f32 {
        let v = 1.0 - u;
        3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u
    }

    fn bezier_x(&self, u: f32) -> f32 {
        Self::comp(self.x1, self.x2, u)
    }

    fn bezier_y(&self, u: f32) -> f32 {
        Self::comp(self.y1, self.y2, u)
    }

    /// dx/du at `u` — for Newton's method.
    fn dx_du(&self, u: f32) -> f32 {
        let v = 1.0 - u;
        3.0 * v * v * self.x1 + 6.0 * v * u * (self.x2 - self.x1) + 3.0 * u * u * (1.0 - self.x2)
    }

    /// Eased progress for linear progress `t`. Solves `bezier_x(u) = t` for `u`
    /// (Newton's method with a bisection fallback), then returns `bezier_y(u)`.
    pub fn ease(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        // Newton-Raphson from u = t.
        let mut u = t;
        for _ in 0..8 {
            let x = self.bezier_x(u) - t;
            if x.abs() < 1e-5 {
                return self.bezier_y(u);
            }
            let d = self.dx_du(u);
            if d.abs() < 1e-6 {
                break;
            }
            u -= x / d;
        }
        // Bisection fallback for ill-conditioned curves.
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        u = t;
        for _ in 0..32 {
            let x = self.bezier_x(u);
            if (x - t).abs() < 1e-5 {
                break;
            }
            if x < t {
                lo = u;
            } else {
                hi = u;
            }
            u = (lo + hi) * 0.5;
        }
        self.bezier_y(u)
    }
}

/// An in-flight tween from `from` to `to` over `duration_ms`, eased by `easing`.
/// Holds owned layouts so the GraphView can drop its live layout reference while
/// the animation runs.
#[derive(Clone, Debug)]
pub struct LayoutTransition {
    pub from: Layout,
    pub to: Layout,
    pub duration_ms: f32,
    pub easing: CubicBezier,
}

impl LayoutTransition {
    pub fn new(from: Layout, to: Layout, duration_ms: f32, easing: CubicBezier) -> Self {
        Self {
            from,
            to,
            duration_ms: duration_ms.max(1.0),
            easing,
        }
    }

    /// Has the tween reached its end by `elapsed_ms`?
    pub fn is_done(&self, elapsed_ms: f32) -> bool {
        elapsed_ms >= self.duration_ms
    }

    /// The interpolated layout at `elapsed_ms`. Each node moves from its `from`
    /// position to its `to` position by the eased fraction. Nodes present only
    /// in `to` (new since the swap began) animate in from their target (no pop);
    /// nodes only in `from` are dropped (the target layout defines membership).
    pub fn sample(&self, elapsed_ms: f32) -> Layout {
        let raw = (elapsed_ms / self.duration_ms).clamp(0.0, 1.0);
        let p = self.easing.ease(raw);
        let mut out = std::collections::HashMap::with_capacity(self.to.len());
        for (id, dst) in self.to.iter() {
            let src = self.from.get(id).unwrap_or(*dst);
            out.insert(
                id.clone(),
                Position {
                    x: src.x + (dst.x - src.x) * p,
                    y: src.y + (dst.y - src.y) * p,
                    z: 0.0,
                },
            );
        }
        Layout::from_positions(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
    fn easing_pins_endpoints() {
        let e = CubicBezier::GRAPH_LAYOUT_SWAP;
        assert_eq!(e.ease(0.0), 0.0);
        assert_eq!(e.ease(1.0), 1.0);
        assert!(
            e.ease(-5.0) == 0.0 && e.ease(5.0) == 1.0,
            "clamps out of range"
        );
    }

    #[test]
    fn easing_is_monotonic_and_solves_x() {
        let e = CubicBezier::GRAPH_LAYOUT_SWAP;
        let mut prev = -1.0;
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let y = e.ease(t);
            assert!(y >= prev - 1e-3, "monotonic non-decreasing at t={t}");
            prev = y;
            assert!((-0.01..=1.01).contains(&y), "stays in [0,1] at t={t}");
        }
    }

    #[test]
    fn linear_bezier_is_identity() {
        // A straight-line control set eases ~linearly.
        let e = CubicBezier::new(0.333, 0.333, 0.667, 0.667);
        assert!((e.ease(0.5) - 0.5).abs() < 1e-2);
    }

    #[test]
    fn sample_at_zero_is_from_at_one_is_to() {
        let t = LayoutTransition::new(
            layout(&[("a", 0.0, 0.0), ("b", 10.0, 0.0)]),
            layout(&[("a", 100.0, 0.0), ("b", 200.0, 0.0)]),
            500.0,
            CubicBezier::GRAPH_LAYOUT_SWAP,
        );
        let start = t.sample(0.0);
        assert_eq!(start.get("a").unwrap().x, 0.0);
        assert_eq!(start.get("b").unwrap().x, 10.0);

        let end = t.sample(500.0);
        assert_eq!(end.get("a").unwrap().x, 100.0);
        assert_eq!(end.get("b").unwrap().x, 200.0);
        assert!(t.is_done(500.0) && !t.is_done(250.0));
    }

    #[test]
    fn sample_midpoint_lies_between() {
        let t = LayoutTransition::new(
            layout(&[("a", 0.0, 0.0)]),
            layout(&[("a", 100.0, 0.0)]),
            500.0,
            CubicBezier::GRAPH_LAYOUT_SWAP,
        );
        let mid = t.sample(250.0).get("a").unwrap().x;
        assert!(mid > 0.0 && mid < 100.0, "midpoint {mid} between endpoints");
    }

    #[test]
    fn position_sequence_is_progressive() {
        // The brief's "verify animation produces the expected position
        // sequence": monotonic advance from `from` toward `to`.
        let t = LayoutTransition::new(
            layout(&[("n", 0.0, 0.0)]),
            layout(&[("n", 1000.0, 0.0)]),
            500.0,
            CubicBezier::GRAPH_LAYOUT_SWAP,
        );
        let xs: Vec<f32> = [0.0, 100.0, 250.0, 400.0, 500.0]
            .iter()
            .map(|&ms| t.sample(ms).get("n").unwrap().x)
            .collect();
        for w in xs.windows(2) {
            assert!(w[1] >= w[0], "x advances monotonically: {:?}", xs);
        }
        assert_eq!(xs.first().copied(), Some(0.0));
        assert_eq!(xs.last().copied(), Some(1000.0));
    }

    #[test]
    fn new_node_animates_in_from_target() {
        // "c" exists only in `to` → it should sit at its target throughout.
        let t = LayoutTransition::new(
            layout(&[("a", 0.0, 0.0)]),
            layout(&[("a", 50.0, 0.0), ("c", 77.0, 88.0)]),
            500.0,
            CubicBezier::GRAPH_LAYOUT_SWAP,
        );
        for ms in [0.0, 250.0, 500.0] {
            let c = t.sample(ms).get("c").unwrap();
            assert_eq!((c.x, c.y), (77.0, 88.0));
        }
    }
}
