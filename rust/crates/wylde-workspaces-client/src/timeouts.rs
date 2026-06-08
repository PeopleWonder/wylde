//! Per-verb timeout tiers (scope v2 §7.2).
//!
//! Three flat tiers — Fast 500ms / Medium 2s / Slow 10s — plus the
//! `symbol_context` per-hop formula `base (200ms) + per_hop (300ms) × N`.
//! Each verb's policy lives in the [`crate::verbs`] table; the client picks
//! the budget from the policy at call time.

use std::time::Duration;

/// Fast tier — registry reads, `list_mru`, cached `symbols.find`, `ping`.
pub const FAST: Duration = Duration::from_millis(500);
/// Medium tier — graph load, notes read/write, `find_by_target`.
pub const MEDIUM: Duration = Duration::from_secs(2);
/// Slow tier — reindex / ingest kicks, deep multi-hop queries.
pub const SLOW: Duration = Duration::from_secs(10);

/// Base budget for the `symbol_context` per-hop formula.
pub const SYMBOL_CONTEXT_BASE: Duration = Duration::from_millis(200);
/// Per-hop increment for the `symbol_context` per-hop formula.
pub const SYMBOL_CONTEXT_PER_HOP: Duration = Duration::from_millis(300);

/// How long a verb is allowed to take before the client gives up on an
/// attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPolicy {
    /// A flat per-attempt budget (the Fast/Medium/Slow tiers).
    Fixed(Duration),
    /// A hop-scaled budget: `base + per_hop × hops`. Used by
    /// `symbol_context`, whose cost grows with neighbourhood depth.
    PerHop { base: Duration, per_hop: Duration },
}

impl TimeoutPolicy {
    /// The Fast tier as a fixed policy.
    pub const fn fast() -> Self {
        TimeoutPolicy::Fixed(FAST)
    }
    /// The Medium tier as a fixed policy.
    pub const fn medium() -> Self {
        TimeoutPolicy::Fixed(MEDIUM)
    }
    /// The Slow tier as a fixed policy.
    pub const fn slow() -> Self {
        TimeoutPolicy::Fixed(SLOW)
    }
    /// The `symbol_context` per-hop policy.
    pub const fn per_hop() -> Self {
        TimeoutPolicy::PerHop {
            base: SYMBOL_CONTEXT_BASE,
            per_hop: SYMBOL_CONTEXT_PER_HOP,
        }
    }

    /// Resolve the budget for a call of `hops` depth. `Fixed` ignores
    /// `hops`; `PerHop` scales by it. A 1-hop `symbol_context` resolves to
    /// 500ms, matching the §2.5 budget.
    pub fn budget(&self, hops: u32) -> Duration {
        match self {
            TimeoutPolicy::Fixed(d) => *d,
            TimeoutPolicy::PerHop { base, per_hop } => *base + *per_hop * hops,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_ignores_hops() {
        assert_eq!(TimeoutPolicy::fast().budget(0), FAST);
        assert_eq!(TimeoutPolicy::fast().budget(9), FAST);
    }

    #[test]
    fn per_hop_one_hop_is_500ms() {
        assert_eq!(
            TimeoutPolicy::per_hop().budget(1),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn per_hop_scales() {
        // 200 + 300*3 = 1100ms
        assert_eq!(
            TimeoutPolicy::per_hop().budget(3),
            Duration::from_millis(1100)
        );
    }
}
