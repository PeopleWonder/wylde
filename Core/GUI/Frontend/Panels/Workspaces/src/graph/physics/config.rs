//! Tunable physics parameters — the **single source of truth** for the force
//! model (Build Order §8: "every tunable lives in exactly one `config.rs`").
//!
//! Nothing here is a layout concern: `level_spacing`, the y-axis direction and
//! the spring rest length live in [`super::super::layout::config`]. This file
//! owns only the *forces* — repulsion strength, spring stiffness, gravity pull,
//! damping, and the equilibrium threshold. C-settings later exposes a subset
//! (`repulsion_strength`, `damping_factor`, …) as user-tunable knobs; until
//! then these defaults are the locked numbers the §2.5 perf budgets assume.
//!
//! Units: positions are model-space px, time is **frames** (`dt == 1.0`), so a
//! velocity of `0.05` means 0.05 px per frame — the equilibrium threshold.

use std::time::Duration;

/// All force-model knobs. `Copy` so the engine and worker each hold their own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsConfig {
    // ── Coulomb repulsion (Barnes-Hut) ──────────────────────────────────
    /// Repulsion constant `k` in `F = k / d²`. Larger → nodes push apart
    /// harder, giving a more spread-out graph.
    pub repulsion_strength: f32,
    /// Beyond this model-space distance (px) repulsion is ignored — the
    /// cutoff radius that keeps far clusters from interacting (Plan v2 §7.10).
    pub cutoff_radius: f32,
    /// Barnes-Hut opening angle `θ`. A quadtree cell is treated as a single
    /// aggregate mass when `cell_size / distance < θ`. `0.0` = exact O(N²);
    /// larger = faster but coarser. `0.85` is the usual accuracy/speed sweet
    /// spot.
    pub theta: f32,
    /// Floor on pair distance (px) so two coincident nodes don't produce an
    /// infinite force on the first frame of a warm start.
    pub min_distance: f32,

    // ── Spring edges (Hooke, asymmetric) ────────────────────────────────
    /// Base spring constant `k` in `F = k · (rest − actual)` when the edge is
    /// **stretched** (actual > rest).
    pub spring_stiffness: f32,
    /// Multiplier applied to `spring_stiffness` when the edge is **compressed**
    /// (actual < rest). > 1 makes compression stiffer than extension so nodes
    /// don't collapse into each other through their edges (brief: asymmetric
    /// stiffness).
    pub spring_compression_factor: f32,

    // ── Bounded gravity (per-level y-target) ────────────────────────────
    /// Spring constant pulling a node's `y` toward its `y_target`
    /// (`F_y = gravity_strength · (y_target − y)`), clamped to
    /// [`Self::max_gravity_force`] so a far-from-target node is *bounded*, not
    /// yanked (brief: bounded gravity). The pull is a **linear spring**
    /// (bounded by construction, unlike a 1/d² law); the clamp is a high safety
    /// rail, kept well above the natural restoring force so it never turns the
    /// spring into a force-saturated limit cycle.
    pub gravity_strength: f32,
    /// Safety cap on the per-frame gravity force magnitude. High by design (see
    /// `gravity_strength`): a clamp near the working force range would make the
    /// y-spring oscillate instead of settle.
    pub max_gravity_force: f32,

    // ── Integration / damping ───────────────────────────────────────────
    /// `velocity *= (1 − damping_factor)` each frame. Default `0.08` settles a
    /// typical graph in ~2 s (≈120 frames) without visible oscillation.
    pub damping_factor: f32,
    /// Per-frame speed clamp (px/frame) — a safety valve so a pathological
    /// force spike can't fling a node off-screen before damping catches it.
    pub max_speed: f32,

    // ── Equilibrium / cooling ───────────────────────────────────────────
    /// When the largest active-node **displacement** drops below this
    /// (px/frame) the simulation is considered settled and freezes
    /// (steady-state < 5 ms — `step` becomes a flag check). Resume on topology
    /// change / drag / camera move past the navigation threshold.
    pub equilibrium_threshold: f32,
    /// Per-frame annealing (cooling) rate. A global `alpha` starts at 1.0 and
    /// decays `alpha *= (1 − alpha_decay)` each frame; the integration step is
    /// scaled by `alpha`, so motion cools to a stop in a **bounded** number of
    /// frames regardless of residual forces. This is what guarantees a ~2 s
    /// settle even for loosely-connected nodes whose 1/d² repulsion tail would
    /// otherwise creep above the velocity threshold forever (the same
    /// alpha-decay schedule d3-force uses). A drag / reload reheats `alpha` to
    /// 1.0. `0.025` → freeze in ~120–150 frames.
    pub alpha_decay: f32,

    // ── Worker cadence / test hooks ─────────────────────────────────────
    /// Target wall-clock between worker frames (≈16 ms → 60 fps). The render
    /// thread reads the latest latched positions regardless, so a slower
    /// worker only means a slower settle, never a dropped render frame.
    pub frame_interval: Duration,
    /// Test-only artificial per-step delay. Lets the off-thread test slow the
    /// worker to prove the render side never blocks on it. `ZERO` in prod.
    pub step_delay: Duration,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            repulsion_strength: 1_000.0,
            cutoff_radius: 200.0,
            theta: 0.85,
            min_distance: 2.0,

            spring_stiffness: 0.04,
            spring_compression_factor: 3.0,

            gravity_strength: 0.04,
            max_gravity_force: 50.0,

            damping_factor: 0.08,
            max_speed: 60.0,

            equilibrium_threshold: 0.05,
            alpha_decay: 0.025,

            frame_interval: Duration::from_millis(16),
            step_delay: Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_locked_force_model() {
        let c = PhysicsConfig::default();
        // The brief / Plan v2 §7.5 locked numbers.
        assert_eq!(c.cutoff_radius, 200.0);
        assert_eq!(c.equilibrium_threshold, 0.05);
        assert_eq!(c.damping_factor, 0.08);
        // Annealing is what bounds the settle time (~2 s); damping alone can't,
        // because a loosely-connected node's 1/d² tail gives a terminal
        // velocity above the threshold.
        assert!(c.alpha_decay > 0.0 && c.alpha_decay < 1.0);
        // Asymmetric stiffness: compression is stiffer than extension.
        assert!(c.spring_compression_factor > 1.0);
        // 60 fps worker cadence.
        assert_eq!(c.frame_interval, Duration::from_millis(16));
        // No artificial delay in prod.
        assert_eq!(c.step_delay, Duration::ZERO);
    }
}
