//! The three force laws, as **pure functions** — no state, no allocation, no
//! threading. The engine ([`super::PhysicsEngine`]) and the Barnes-Hut tree
//! ([`super::barnes_hut`]) call these; tests exercise them in isolation
//! (Build Order §8: "physics simulates; tests run it without UI").
//!
//! All forces are 2D `(fx, fy)` in model px/frame²; `z` is untouched in v1.

/// **Bounded gravity** toward a per-node y-target (brief / Plan v2 §7.5).
///
/// A spring pulls `y` toward `y_target`: `F_y = k · (y_target − y)`, then
/// clamped to `±max` so a node far from its level is pulled *firmly but not
/// explosively* — the "bounded" in bounded gravity. Only the y component is
/// affected; depth bands are horizontal.
#[inline]
pub fn gravity(y: f32, y_target: f32, k: f32, max: f32) -> f32 {
    (k * (y_target - y)).clamp(-max, max)
}

/// **Coulomb repulsion** between two bodies: `F = k / d²` directed from
/// `other` toward `self` (push apart). Returns the force on the *self* body.
///
/// `min_distance` floors `d` so coincident warm-start nodes don't blow up;
/// beyond `cutoff` the force is zero (the cutoff radius, Plan v2 §7.10).
/// `charge` is the aggregate mass of `other` (1.0 for a single node, N for a
/// Barnes-Hut cell of N nodes).
#[inline]
pub fn coulomb(
    dx: f32,
    dy: f32,
    charge: f32,
    k: f32,
    min_distance: f32,
    cutoff: f32,
) -> (f32, f32) {
    let dist_sq = dx * dx + dy * dy;
    if dist_sq > cutoff * cutoff {
        return (0.0, 0.0);
    }
    let dist = dist_sq.sqrt().max(min_distance);
    // Magnitude k·charge / d², projected onto the unit separation vector
    // (dx,dy)/dist → k·charge / d³ · (dx,dy).
    let inv = (k * charge) / (dist * dist * dist);
    (dx * inv, dy * inv)
}

/// **Spring edge** (Hooke, asymmetric). Returns the scalar force magnitude
/// along the edge: positive = the endpoints should move *apart* (edge
/// compressed, actual < rest), negative = move *together* (stretched).
///
/// Asymmetric stiffness (brief): compression uses
/// `k · compression_factor` (stiffer), extension uses `k`. This stops nodes
/// from collapsing into each other through a shared edge while still letting
/// long edges reel their endpoints in gently.
#[inline]
pub fn spring(rest: f32, actual: f32, k: f32, compression_factor: f32) -> f32 {
    let displacement = rest - actual; // >0 compressed, <0 stretched
    if displacement > 0.0 {
        displacement * k * compression_factor
    } else {
        displacement * k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gravity_pulls_toward_target_and_is_bounded() {
        // Below target (y < y_target) → positive (pull down toward larger y).
        let f = gravity(0.0, 100.0, 0.03, 4.0);
        assert!(f > 0.0);
        // Above target → negative.
        assert!(gravity(200.0, 100.0, 0.03, 4.0) < 0.0);
        // At target → zero.
        assert_eq!(gravity(100.0, 100.0, 0.03, 4.0), 0.0);
        // Bounded: a huge offset is clamped to ±max, not k·offset.
        assert_eq!(gravity(0.0, 1_000_000.0, 0.03, 4.0), 4.0);
        assert_eq!(gravity(1_000_000.0, 0.0, 0.03, 4.0), -4.0);
    }

    #[test]
    fn coulomb_is_symmetric_and_inverse_square() {
        // Two nodes on the x-axis: B at +10 from A pushes A in −x.
        let (fx, fy) = coulomb(-10.0, 0.0, 1.0, 1_000.0, 1.0, 1_000.0);
        assert!(fx < 0.0 && fy.abs() < 1e-6, "pushed along −x");

        // Symmetry: the force on B from A is equal and opposite.
        let (fx2, fy2) = coulomb(10.0, 0.0, 1.0, 1_000.0, 1.0, 1_000.0);
        assert!((fx + fx2).abs() < 1e-4 && (fy + fy2).abs() < 1e-4);

        // Inverse-square: doubling the distance quarters the magnitude.
        let near = coulomb(10.0, 0.0, 1.0, 1_000.0, 1.0, 1_000.0).0.abs();
        let far = coulomb(20.0, 0.0, 1.0, 1_000.0, 1.0, 1_000.0).0.abs();
        let ratio = near / far;
        assert!((ratio - 4.0).abs() < 1e-2, "ratio {ratio} ≈ 4");
    }

    #[test]
    fn coulomb_respects_cutoff_and_min_distance() {
        // Beyond cutoff → exactly zero.
        assert_eq!(coulomb(300.0, 0.0, 1.0, 1_000.0, 2.0, 200.0), (0.0, 0.0));
        // Coincident nodes: floored at min_distance, finite force (no NaN/inf).
        let (fx, fy) = coulomb(0.0, 0.0, 1.0, 1_000.0, 2.0, 200.0);
        assert!(fx.is_finite() && fy.is_finite());
    }

    #[test]
    fn coulomb_aggregate_charge_scales_linearly() {
        let one = coulomb(10.0, 0.0, 1.0, 1_000.0, 1.0, 1_000.0).0;
        let five = coulomb(10.0, 0.0, 5.0, 1_000.0, 1.0, 1_000.0).0;
        assert!((five / one - 5.0).abs() < 1e-3);
    }

    #[test]
    fn spring_is_asymmetric() {
        let rest = 100.0;
        let k = 0.01;
        let cf = 3.0;
        // Compressed by 10 (actual 90) → push apart, 3× stiffer.
        let compressed = spring(rest, 90.0, k, cf);
        // Stretched by 10 (actual 110) → pull together, base stiffness.
        let stretched = spring(rest, 110.0, k, cf);
        assert!(compressed > 0.0, "compressed pushes apart");
        assert!(stretched < 0.0, "stretched pulls together");
        // Same 10px displacement, but compression is `compression_factor`×
        // stronger in magnitude.
        assert!((compressed.abs() / stretched.abs() - cf).abs() < 1e-4);
        // At rest → no force.
        assert_eq!(spring(rest, rest, k, cf), 0.0);
    }
}
