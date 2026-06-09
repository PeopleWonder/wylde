//! The animated layout-swap machinery (Slice C-layout).
//!
//! Split out of `graph/mod.rs` (2026-06-09 pre-C-navigation cleanup); no
//! behaviour change. The pure position tween lives in
//! [`layout::transition`](super::layout); this file owns the *driver*: pairing
//! a tween with a wall-clock start, advancing it at ~60 fps on the gpui main
//! thread, and finalising physics (resume / leave-paused) on completion.
//!
//! C-navigation's camera tweens are a separate concern and land at
//! `navigation/transition.rs` per Build Order §4 — this driver stays
//! layout-swap-only.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Context};

use super::layout::{CubicBezier, LayoutKind, LayoutTransition};
use super::GraphView;

/// An in-flight animated layout swap (Slice C-layout). The pure tween lives in
/// [`LayoutTransition`]; this pairs it with a wall-clock start and the target
/// layout so the driver can finalise (resume / leave-paused physics) on
/// completion.
pub(super) struct ActiveTransition {
    pub(super) anim: LayoutTransition,
    pub(super) start: Instant,
    pub(super) target: LayoutKind,
}

/// Result of advancing the layout-swap tween one step.
pub(super) enum TransitionStep {
    Running,
    Completed,
}

/// Tween tick cadence (~60 fps). The animation runs on the gpui main thread,
/// independent of the physics worker's own frame interval.
const TRANSITION_FRAME: Duration = Duration::from_millis(16);

impl GraphView {
    /// Switch to `kind` with the locked 500 ms animated tween (Visual Style v1
    /// `graph_layout_swap`). No-op if already showing `kind`. Cycled by
    /// `Ctrl+Shift+L` via [`LayoutKind::next`].
    pub(super) fn set_layout(&mut self, kind: LayoutKind, cx: &mut Context<Self>) {
        if !self.begin_layout_swap(kind, Instant::now()) {
            return;
        }
        self.spawn_transition_driver(cx);
        cx.notify();
    }

    /// Pure core of [`set_layout`](Self::set_layout): snapshot the current
    /// positions as `from`, compute the target backend's positions as `to`,
    /// pause physics, and arm the tween. Returns `false` (no swap armed) when
    /// already on `kind`. `now` is injected so tests drive the animation
    /// deterministically.
    pub(super) fn begin_layout_swap(&mut self, kind: LayoutKind, now: Instant) -> bool {
        if kind == self.current_layout && self.transition.is_none() {
            return false;
        }
        let from = (*self.layout).clone();
        let to = kind.compute_positions(self.graph.as_ref());
        // Pause physics for the swap. Deterministic targets never resume it;
        // force-directed resumes (seeded from `to`) when the tween completes.
        self.physics = None;
        let (duration_ms, easing) = self.swap_anim();
        self.transition = Some(ActiveTransition {
            anim: LayoutTransition::new(from, to, duration_ms, easing),
            start: now,
            target: kind,
        });
        self.current_layout = kind;
        if let Some(id) = &self.workspace_id {
            self.layout_cache.insert(id.clone(), kind);
        }
        true
    }

    /// Advance the in-flight tween to wall-clock `now`. Updates `self.layout`;
    /// on completion finalises — force-directed respawns the worker seeded from
    /// the target positions, deterministic layouts leave it paused. cx-free so
    /// tests step it directly; the gpui driver re-attaches the subscription.
    pub(super) fn advance_transition(&mut self, now: Instant) -> TransitionStep {
        let (layout, done, target, final_to) = {
            let Some(t) = self.transition.as_ref() else {
                return TransitionStep::Completed;
            };
            let elapsed = now.saturating_duration_since(t.start).as_secs_f32() * 1000.0;
            if t.anim.is_done(elapsed) {
                (t.anim.to.clone(), true, t.target, Some(t.anim.to.clone()))
            } else {
                (t.anim.sample(elapsed), false, t.target, None)
            }
        };
        self.layout = Rc::new(layout);
        if !done {
            return TransitionStep::Running;
        }
        self.transition = None;
        self.physics = if target.is_physics() {
            self.spawn_worker(final_to.as_ref())
        } else {
            None
        };
        TransitionStep::Completed
    }

    /// Drive the tween on the gpui main thread: a ~60 fps timer feeds wall-clock
    /// into [`advance_transition`](Self::advance_transition) until it completes,
    /// then re-attaches the physics subscription (a no-op for a paused layout).
    pub(super) fn spawn_transition_driver(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                app_cx.background_executor().timer(TRANSITION_FRAME).await;
                let running = this.update(app_cx, |view, cx| {
                    let step = view.advance_transition(Instant::now());
                    cx.notify();
                    matches!(step, TransitionStep::Running)
                });
                match running {
                    Ok(true) => continue,
                    Ok(false) => {
                        let _ = this.update(app_cx, |view, cx| view.subscribe_physics(cx));
                        break;
                    }
                    Err(_) => break, // the view is gone
                }
            }
        })
        .detach();
    }

    /// The layout-swap duration + easing, read FROM the theme
    /// (`animations.graph_layout_swap`); the fallback (used only when the theme
    /// failed to load) equals the locked spec value, so a swap still animates.
    fn swap_anim(&self) -> (f32, CubicBezier) {
        self.theme
            .as_ref()
            .and_then(|t| t.animation("graph_layout_swap"))
            .map(|a| (a.duration_ms, CubicBezier::from_array(a.easing)))
            .unwrap_or((500.0, CubicBezier::GRAPH_LAYOUT_SWAP))
    }
}
