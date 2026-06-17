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
    ///
    /// Only applied when [`Self::use_radial`] is `false` (the legacy banded
    /// layout). In the default center-anchor layout the [`Self::radial_strength`]
    /// force replaces it.
    pub gravity_strength: f32,
    /// Safety cap on the per-frame gravity / radial force magnitude. High by
    /// design (see `gravity_strength`): a clamp near the working force range
    /// would make the restoring spring oscillate instead of settle.
    pub max_gravity_force: f32,

    // ── Center-anchor layout (viz-fix: centre-of-mass + radial-by-depth) ──
    /// **Centre-of-mass centering** (d3 `forceCenter` style). Each frame the
    /// mean position of the active, non-pinned nodes is computed and a uniform
    /// force `−center_strength · mean` is applied to every such node, pinning
    /// the whole graph's centroid at the origin and killing the global drift
    /// the old y-only model suffered (nothing constrained x). It is a *uniform
    /// translation* — it never changes nodes' relative positions, so it can't
    /// fight the structural forces; it only stops the cloud from wandering.
    /// `0.0` disables it. One mean pass + one apply pass per frame — O(N), no
    /// allocation.
    pub center_strength: f32,
    /// **Radial-by-depth attraction**. When [`Self::use_radial`] is `true`
    /// (default), each node is pulled toward the circle of radius
    /// `depth · ring_spacing` centred at the origin
    /// (`F_r = radial_strength · (r_target − r)` along the radial unit vector,
    /// clamped to [`Self::max_gravity_force`]). Depth-0 roots collapse to the
    /// centre; dependencies fan out in concentric rings — the "everything
    /// derives from a centre" structure. Crucially this is also the **island
    /// tether**: every node (connected or not) has an *absolute* radial target
    /// at a fixed origin, so a disconnected component can no longer be flung to
    /// infinity by repulsion — it orbits the centre instead. This subsumes the
    /// separate "pinned hub + springs from component roots" mechanism (a hub at
    /// the origin is exactly what r_target = 0 anchors the roots to) without a
    /// synthetic render node. `0.0` disables it.
    pub radial_strength: f32,
    /// Radius (model px) added per dependency level for the radial force
    /// (`r_target = depth · ring_spacing`). Mirrors the layout `level_spacing`.
    pub ring_spacing: f32,
    /// Use the radial-by-depth force (concentric rings around the origin,
    /// [`Self::radial_strength`]) instead of the y-only bounded gravity
    /// (top-down depth bands, [`Self::gravity_strength`]). Default `true` — the
    /// center-anchor layout Aaron's directive calls for. Set `false` for the
    /// legacy banded layout.
    pub use_radial: bool,

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
            // Rebalanced for the center-anchor layout (viz-fix): the old
            // repulsion `1000` with springs at only `0.04` let repulsion
            // dominate and sprawl the graph the (absent) centering couldn't
            // counter. Lower repulsion + stiffer springs let the new centering
            // / radial forces win, settling a compact, centred map.
            repulsion_strength: 450.0, // was 1000
            cutoff_radius: 200.0,
            theta: 0.85,
            min_distance: 2.0,

            spring_stiffness: 0.08, // was 0.04
            spring_compression_factor: 3.0,

            gravity_strength: 0.04,
            max_gravity_force: 50.0,

            center_strength: 0.03,
            radial_strength: 0.05,
            ring_spacing: 120.0,
            use_radial: true,

            damping_factor: 0.08,
            max_speed: 60.0,

            equilibrium_threshold: 0.05,
            alpha_decay: 0.025,

            frame_interval: Duration::from_millis(16),
            step_delay: Duration::ZERO,
        }
    }
}

/// Above this node count the default force model (tuned for ~1.5 k nodes)
/// makes a 10 k-node body bounce hard and settle slowly — the "spiky, sprawly"
/// feel. [`PhysicsConfig::for_node_count`] swaps in a calmer profile past this
/// threshold. The cluster-level body (G3) stays small and keeps the default.
pub const LARGE_GRAPH_THRESHOLD: usize = 5_000;

impl PhysicsConfig {
    /// The force profile to run for a body of `node_count` nodes.
    ///
    /// Small/medium graphs keep the locked default. Past
    /// [`LARGE_GRAPH_THRESHOLD`] we tame the model (visual-polish G4): lower
    /// repulsion + a wider cutoff spreads the push more gently over more
    /// neighbours instead of a few hard kicks; heavier damping and a relaxed
    /// equilibrium threshold plus a faster cool let the cloud settle quickly
    /// into a tight map rather than creeping. Values are a starting point —
    /// they want an empirical feel-test against the real 10 k graph and, once
    /// happy, promotion into `GraphProfile` (G6) so they're tunable without a
    /// rebuild.
    pub fn for_node_count(node_count: usize) -> Self {
        let base = Self::default();
        if node_count <= LARGE_GRAPH_THRESHOLD {
            return base;
        }
        Self {
            repulsion_strength: 300.0,   // 450 → 300: softer push at scale
            cutoff_radius: 300.0,        // 200 → 300: spread the push wider
            ring_spacing: 160.0,         // 120 → 160: more room between rings
            damping_factor: 0.14,        // 0.08 → 0.14: bleed motion faster
            equilibrium_threshold: 0.10, // 0.05 → 0.10: accept a calmer settle
            alpha_decay: 0.045,          // 0.025 → 0.045: cool to a stop sooner
            ..base
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_graphs_keep_the_default_profile() {
        // At or below the threshold, nothing changes — the locked numbers hold.
        assert_eq!(PhysicsConfig::for_node_count(0), PhysicsConfig::default());
        assert_eq!(
            PhysicsConfig::for_node_count(LARGE_GRAPH_THRESHOLD),
            PhysicsConfig::default()
        );
    }

    #[test]
    fn large_graphs_get_a_calmer_profile() {
        let big = PhysicsConfig::for_node_count(LARGE_GRAPH_THRESHOLD + 1);
        let def = PhysicsConfig::default();
        // Softer push, wider cutoff, heavier damping, relaxed equilibrium,
        // faster cool — the whole point is "calmer at scale".
        assert!(big.repulsion_strength < def.repulsion_strength);
        assert!(big.cutoff_radius > def.cutoff_radius);
        assert!(big.damping_factor > def.damping_factor);
        assert!(big.equilibrium_threshold > def.equilibrium_threshold);
        assert!(big.alpha_decay > def.alpha_decay);
        // Untouched knobs stay at the locked default.
        assert_eq!(big.spring_stiffness, def.spring_stiffness);
        assert_eq!(big.frame_interval, def.frame_interval);
    }

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

    #[test]
    fn center_anchor_layout_is_the_default() {
        let c = PhysicsConfig::default();
        // Radial-by-depth is the default layout (Aaron's center-anchor
        // directive); the centering + radial pulls are both armed.
        assert!(c.use_radial);
        assert!(c.center_strength > 0.0, "centering force armed");
        assert!(c.radial_strength > 0.0, "radial force armed");
        assert!(c.ring_spacing > 0.0);
        // The charge/link rebalance: repulsion dropped and springs stiffened so
        // centering can win over the old repulsion-dominated sprawl.
        assert!(
            c.repulsion_strength < 1_000.0,
            "repulsion lowered from 1000"
        );
        assert!(c.spring_stiffness > 0.04, "springs stiffened from 0.04");
    }
}
