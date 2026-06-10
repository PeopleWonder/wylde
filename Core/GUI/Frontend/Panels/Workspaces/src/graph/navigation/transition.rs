//! Camera tweens (Slice C-navigation) — the animated zoom-into-cluster /
//! zoom-out moves, distinct from the layout-swap tween
//! (`graph/transition_driver.rs` + `layout/transition.rs`), which morphs node
//! positions. A camera tween moves only the [`Camera`]; node positions (and
//! the physics worker) are untouched while it runs.
//!
//! The pure tween ([`CameraTween`]) is gpui-free and unit-tested directly;
//! the driver half (`impl GraphView`) mirrors the layout-swap driver: a
//! ~60 fps main-thread timer feeds wall-clock into
//! [`GraphView::advance_camera_tween`]. Duration + easing are read FROM the
//! Theme (`animations.graph_zoom_into_cluster` 400 ms /
//! `animations.graph_zoom_out` 380 ms); the fallbacks equal the locked spec
//! so a theme load failure still animates correctly.

use std::time::{Duration, Instant};

use gpui::{AsyncApp, Context};

use super::super::layout::CubicBezier;
use super::super::GraphView;
use crate::graph::render::Camera;

/// The locked `graph_zoom_into_cluster` / `graph_zoom_out` easing
/// (`easeInOutCubic` per Visual Style v1) — degrade fallback only.
pub const EASE_IN_OUT_CUBIC: CubicBezier = CubicBezier::new(0.645, 0.045, 0.355, 1.0);

/// Locked fallback durations (Visual Style v1), used only when the theme
/// failed to load.
pub const ZOOM_IN_FALLBACK_MS: f32 = 400.0;
pub const ZOOM_OUT_FALLBACK_MS: f32 = 380.0;

/// A pure camera tween from one [`Camera`] to another. Pan interpolates
/// linearly; zoom interpolates **geometrically** (log-space) so a 4× zoom
/// move feels constant-rate rather than slow-then-explosive — the standard
/// map-zoom interpolation.
#[derive(Clone, Copy, Debug)]
pub struct CameraTween {
    pub from: Camera,
    pub to: Camera,
    pub duration_ms: f32,
    pub easing: CubicBezier,
}

impl CameraTween {
    pub fn new(from: Camera, to: Camera, duration_ms: f32, easing: CubicBezier) -> Self {
        CameraTween {
            from,
            to,
            duration_ms: duration_ms.max(1.0),
            easing,
        }
    }

    pub fn is_done(&self, elapsed_ms: f32) -> bool {
        elapsed_ms >= self.duration_ms
    }

    /// The interpolated camera at `elapsed_ms`.
    pub fn sample(&self, elapsed_ms: f32) -> Camera {
        let raw = (elapsed_ms / self.duration_ms).clamp(0.0, 1.0);
        let p = self.easing.ease(raw);
        // Geometric zoom (both endpoints are clamped > 0 by the camera).
        let zoom = self.from.zoom * (self.to.zoom / self.from.zoom).powf(p);
        Camera {
            pan_x: self.from.pan_x + (self.to.pan_x - self.from.pan_x) * p,
            pan_y: self.from.pan_y + (self.to.pan_y - self.from.pan_y) * p,
            zoom,
        }
    }
}

/// An in-flight camera tween paired with its wall-clock start.
pub(in crate::graph) struct ActiveCameraTween {
    pub anim: CameraTween,
    pub start: Instant,
}

/// Result of advancing the camera tween one step.
pub(in crate::graph) enum CameraStep {
    Running,
    Completed,
}

/// Tween tick cadence (~60 fps), matching the layout-swap driver.
const CAMERA_FRAME: Duration = Duration::from_millis(16);

impl GraphView {
    /// Arm a camera tween from the current camera to `to`, with duration +
    /// easing from the theme animation `key` (falling back to the locked spec
    /// values). `now` injected so tests drive it deterministically. Replaces
    /// any in-flight camera tween (the new move takes over from wherever the
    /// camera currently is).
    pub(in crate::graph) fn begin_camera_tween(&mut self, to: Camera, key: &str, now: Instant) {
        let (duration_ms, easing) = self.camera_anim(key);
        self.camera_transition = Some(ActiveCameraTween {
            anim: CameraTween::new(self.camera, to, duration_ms, easing),
            start: now,
        });
    }

    /// Advance the in-flight camera tween to wall-clock `now`. Updates
    /// `self.camera`; on completion clears the tween, refreshes the physics
    /// cull region, and — when an exit-edge jump queued a follow-up — enters
    /// the pending target cluster (arming the zoom-in tween). cx-free so tests
    /// step it directly.
    pub(in crate::graph) fn advance_camera_tween(&mut self, now: Instant) -> CameraStep {
        let Some(t) = self.camera_transition.as_ref() else {
            return CameraStep::Completed;
        };
        let elapsed = now.saturating_duration_since(t.start).as_secs_f32() * 1000.0;
        let done = t.anim.is_done(elapsed);
        self.camera = if done {
            t.anim.to
        } else {
            t.anim.sample(elapsed)
        };
        if !done {
            return CameraStep::Running;
        }
        self.camera_transition = None;
        self.push_viewport();
        // Exit-edge jump second phase: zoom back in, into the target cluster.
        if let Some(target) = self.pending_enter.take() {
            self.enter_cluster_by_id(&target, now);
        }
        match self.camera_transition {
            Some(_) => CameraStep::Running,
            None => CameraStep::Completed,
        }
    }

    /// Drive the camera tween on the gpui main thread at ~60 fps until it
    /// completes (including any chained pending-enter phase). Safe to call
    /// when a driver is already running: the existing loop keeps advancing
    /// whatever tween is current, and a second loop exits on the first
    /// already-completed step.
    pub(in crate::graph) fn spawn_camera_driver(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                app_cx.background_executor().timer(CAMERA_FRAME).await;
                let running = this.update(app_cx, |view, cx| {
                    let step = view.advance_camera_tween(Instant::now());
                    cx.notify();
                    matches!(step, CameraStep::Running)
                });
                match running {
                    Ok(true) => continue,
                    _ => break, // completed, or the view is gone
                }
            }
        })
        .detach();
    }

    /// Duration + easing for a theme animation `key`, with the locked-spec
    /// fallback when the theme failed to load or lacks the key.
    fn camera_anim(&self, key: &str) -> (f32, CubicBezier) {
        let fallback_ms = if key == "graph_zoom_out" {
            ZOOM_OUT_FALLBACK_MS
        } else {
            ZOOM_IN_FALLBACK_MS
        };
        self.theme
            .as_ref()
            .and_then(|t| t.animation(key))
            .map(|a| (a.duration_ms, CubicBezier::from_array(a.easing)))
            .unwrap_or((fallback_ms, EASE_IN_OUT_CUBIC))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(pan_x: f32, pan_y: f32, zoom: f32) -> Camera {
        Camera { pan_x, pan_y, zoom }
    }

    #[test]
    fn sample_pins_endpoints() {
        let t = CameraTween::new(
            cam(0.0, 0.0, 1.0),
            cam(-100.0, 40.0, 4.0),
            400.0,
            EASE_IN_OUT_CUBIC,
        );
        let s0 = t.sample(0.0);
        assert_eq!((s0.pan_x, s0.pan_y, s0.zoom), (0.0, 0.0, 1.0));
        let s1 = t.sample(400.0);
        assert!((s1.pan_x + 100.0).abs() < 1e-3);
        assert!((s1.zoom - 4.0).abs() < 1e-3);
        assert!(t.is_done(400.0) && !t.is_done(399.0));
    }

    #[test]
    fn zoom_interpolates_geometrically() {
        // 1 → 4 zoom: log-space midpoint is 2, not the arithmetic 2.5. The
        // eased curve passes p=0.5 at raw=0.5 for a symmetric easing.
        let t = CameraTween::new(
            cam(0.0, 0.0, 1.0),
            cam(0.0, 0.0, 4.0),
            400.0,
            EASE_IN_OUT_CUBIC,
        );
        let mid = t.sample(200.0);
        assert!(
            (mid.zoom - 2.0).abs() < 0.05,
            "geometric midpoint ≈ 2, got {}",
            mid.zoom
        );
    }

    #[test]
    fn zoom_advance_is_monotonic() {
        let t = CameraTween::new(
            cam(0.0, 0.0, 0.5),
            cam(0.0, 0.0, 6.0),
            400.0,
            EASE_IN_OUT_CUBIC,
        );
        let mut prev = 0.0;
        for i in 0..=20 {
            let z = t.sample(i as f32 * 20.0).zoom;
            assert!(z >= prev - 1e-4, "zoom advances monotonically");
            prev = z;
        }
    }

    #[test]
    fn zoom_out_tween_descends() {
        let t = CameraTween::new(
            cam(0.0, 0.0, 5.0),
            cam(0.0, 0.0, 0.8),
            380.0,
            EASE_IN_OUT_CUBIC,
        );
        assert!(t.sample(190.0).zoom < 5.0);
        assert!((t.sample(380.0).zoom - 0.8).abs() < 1e-3);
    }

    #[test]
    fn degenerate_duration_clamps() {
        let t = CameraTween::new(
            cam(0.0, 0.0, 1.0),
            cam(0.0, 0.0, 2.0),
            0.0,
            EASE_IN_OUT_CUBIC,
        );
        // duration clamps to ≥ 1 ms; sampling past it lands on the target.
        let s = t.sample(5.0);
        assert!((s.zoom - 2.0).abs() < 1e-3);
    }

    #[test]
    fn tween_sampling_is_cheap() {
        // Perf sanity: one sample is a handful of float ops; 10k samples must
        // be far under a frame even in debug builds.
        let t = CameraTween::new(
            cam(10.0, -3.0, 0.7),
            cam(-200.0, 90.0, 5.0),
            400.0,
            EASE_IN_OUT_CUBIC,
        );
        let start = std::time::Instant::now();
        let mut acc = 0.0f32;
        for i in 0..10_000 {
            acc += t.sample((i % 400) as f32).zoom;
        }
        assert!(acc > 0.0);
        assert!(
            start.elapsed().as_millis() < 16,
            "10k tween samples inside one frame budget (got {:?})",
            start.elapsed()
        );
    }
}
