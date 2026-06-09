//! Velocity damping + equilibrium detection — the two pieces that turn the
//! force model into something that *settles* instead of oscillating forever.
//!
//! Damping bleeds kinetic energy each frame so the graph reaches a steady
//! state; equilibrium detection watches the peak speed and flips a freeze flag
//! once everything is essentially still, so a settled graph costs ~nothing to
//! "simulate" (steady-state < 5 ms, Plan v2 §2.5).

/// Apply per-frame velocity damping: `v *= (1 − factor)`. `factor` is clamped
/// to `[0, 1]` so a mis-set config can't amplify (`< 0`) or invert (`> 1`).
#[inline]
pub fn damp(v: f32, factor: f32) -> f32 {
    v * (1.0 - factor.clamp(0.0, 1.0))
}

/// Clamp a per-frame speed to `max` (the safety valve against force spikes).
/// Returns the scaled `(vx, vy)`.
#[inline]
pub fn clamp_speed(vx: f32, vy: f32, max: f32) -> (f32, f32) {
    let speed_sq = vx * vx + vy * vy;
    if speed_sq > max * max && speed_sq > 0.0 {
        let scale = max / speed_sq.sqrt();
        (vx * scale, vy * scale)
    } else {
        (vx, vy)
    }
}

/// Tracks the largest node speed seen during a step and decides whether the
/// simulation has settled. Stateless between steps apart from the running max —
/// the engine resets it each step via [`Equilibrium::reset`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Equilibrium {
    max_speed: f32,
}

impl Equilibrium {
    /// Start a fresh step's measurement.
    pub fn reset(&mut self) {
        self.max_speed = 0.0;
    }

    /// Fold one node's speed (px/frame) into the running maximum.
    #[inline]
    pub fn observe(&mut self, speed: f32) {
        if speed > self.max_speed {
            self.max_speed = speed;
        }
    }

    /// The peak speed observed since the last [`reset`](Self::reset).
    pub fn max_speed(&self) -> f32 {
        self.max_speed
    }

    /// Has the simulation settled? True when the peak speed is below the
    /// equilibrium threshold (default 0.05 px/frame).
    pub fn is_settled(&self, threshold: f32) -> bool {
        self.max_speed < threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damping_reduces_velocity() {
        // 8% damping → 92% retained.
        assert!((damp(10.0, 0.08) - 9.2).abs() < 1e-5);
        // Zero damping is a no-op.
        assert_eq!(damp(10.0, 0.0), 10.0);
        // Over-damping is clamped, never inverts the sign.
        assert_eq!(damp(10.0, 2.0), 0.0);
        // Negative factor can't amplify.
        assert_eq!(damp(10.0, -1.0), 10.0);
    }

    #[test]
    fn damping_is_monotone_toward_zero() {
        let mut v = 100.0;
        for _ in 0..200 {
            v = damp(v, 0.08);
        }
        assert!(v.abs() < 0.05, "200 frames of damping → near zero, got {v}");
    }

    #[test]
    fn speed_clamp_caps_magnitude_preserving_direction() {
        let (vx, vy) = clamp_speed(30.0, 40.0, 10.0); // speed 50 → 10
        let speed = (vx * vx + vy * vy).sqrt();
        assert!((speed - 10.0).abs() < 1e-4);
        // Direction preserved (3:4 ratio).
        assert!((vx / vy - 0.75).abs() < 1e-4);
        // Under the cap → untouched.
        assert_eq!(clamp_speed(1.0, 2.0, 10.0), (1.0, 2.0));
    }

    #[test]
    fn equilibrium_flips_at_threshold() {
        let mut e = Equilibrium::default();
        e.reset();
        e.observe(0.2);
        e.observe(0.01);
        e.observe(0.04);
        // Peak is 0.2 → not settled at threshold 0.05.
        assert_eq!(e.max_speed(), 0.2);
        assert!(!e.is_settled(0.05));

        e.reset();
        e.observe(0.04);
        e.observe(0.01);
        // Peak now 0.04 < 0.05 → settled.
        assert!(e.is_settled(0.05));
    }
}
