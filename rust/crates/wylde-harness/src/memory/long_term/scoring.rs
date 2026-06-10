//! Importance + recency-decay scoring shared across long-term memory.
//!
//! Rust port of `Core/harness/memory/scoring.py`. Stays a pure function
//! library — no IO, no state — so it's trivially testable and easy to
//! reuse from future workspace / short-term tiers.
//!
//! Formula (the Wylde user's spec):
//!
//! ```text
//! score = similarity * (importance / 10) * exp(-age_days / decay)
//! ```

/// Default decay constant in days. Matches Python.
pub const DEFAULT_DECAY_DAYS: f64 = 30.0;

const SECONDS_PER_DAY: f64 = 86_400.0;

/// Combined score = `similarity * importance_norm * exp(-age_days / decay)`.
///
/// `importance` is divided by 10 (cap at 1.0) so the LLM's 0..10 scale
/// stays inspectable in the Settings UI while the math operates on a
/// 0..1 weight. `now` defaults to `SystemTime::now()` if `None`.
pub fn combined_score(
    similarity: f64,
    importance: f64,
    last_used_at: f64,
    decay_days: f64,
    now: Option<f64>,
) -> f64 {
    let now = now.unwrap_or_else(now_secs);
    let age_seconds = (now - last_used_at).max(0.0);
    let age_days = age_seconds / SECONDS_PER_DAY;
    let importance_norm = (importance / 10.0).clamp(0.0, 1.0);
    let decay = (-age_days / decay_days.max(1e-6)).exp();
    similarity * importance_norm * decay
}

/// Crude importance estimator for memories the LLM didn't tag.
///
/// Capped at 8 — the 9..10 band is reserved for hand-flagged identity /
/// hard preferences. Matches Python's `heuristic_importance`.
pub fn heuristic_importance(body: &str, entity_count: usize) -> i32 {
    let length_pts = (body.len() / 100).min(4) as i32;
    let entity_pts = (entity_count as i32).clamp(0, 3);
    let score = 3 + length_pts + entity_pts;
    score.clamp(1, 8)
}

/// Coerce an LLM-supplied importance to the int 1..10 range.
///
/// Accepts anything that parses as a float; falls back to
/// [`heuristic_importance`] on `None`, NaN, or non-numeric input.
/// Mirrors Python's `normalize_importance`.
pub fn normalize_importance(raw: Option<f64>, body: &str, entity_count: usize) -> i32 {
    match raw {
        Some(n) if !n.is_nan() => (n.round() as i32).clamp(1, 10),
        _ => heuristic_importance(body, entity_count),
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn combined_score_at_zero_age_is_similarity_times_importance_norm() {
        let now = 1000.0;
        let s = combined_score(0.5, 8.0, now, 30.0, Some(now));
        // 0.5 * 0.8 * exp(0) = 0.4
        assert!(approx(s, 0.4));
    }

    #[test]
    fn combined_score_decays_with_age() {
        let now = 1000.0;
        let one_day_ago = now - SECONDS_PER_DAY;
        let fresh = combined_score(0.5, 8.0, now, 30.0, Some(now));
        let stale = combined_score(0.5, 8.0, one_day_ago, 30.0, Some(now));
        assert!(stale < fresh);
    }

    #[test]
    fn combined_score_clamps_importance_to_one() {
        let now = 1000.0;
        // importance 50 → clamp to 1.0 (5.0 / 10 = 0.5 → wait, 50/10=5
        // → clamp to 1.0).
        let s = combined_score(1.0, 50.0, now, 30.0, Some(now));
        assert!(approx(s, 1.0));
    }

    #[test]
    fn heuristic_importance_caps_at_eight() {
        // Huge body + many entities should still cap at 8.
        let body = "x".repeat(10_000);
        let s = heuristic_importance(&body, 100);
        assert_eq!(s, 8);
    }

    #[test]
    fn heuristic_importance_returns_three_for_empty_body() {
        assert_eq!(heuristic_importance("", 0), 3);
    }

    #[test]
    fn normalize_importance_passes_through_numeric_clamped_to_one_to_ten() {
        assert_eq!(normalize_importance(Some(0.0), "x", 0), 1);
        assert_eq!(normalize_importance(Some(7.4), "x", 0), 7);
        assert_eq!(normalize_importance(Some(7.6), "x", 0), 8);
        assert_eq!(normalize_importance(Some(50.0), "x", 0), 10);
        assert_eq!(normalize_importance(Some(-3.0), "x", 0), 1);
    }

    #[test]
    fn normalize_importance_falls_back_to_heuristic_when_none() {
        // length 500 → length_pts = min(4, 5) = 4; entity_count=2 →
        // entity_pts = 2. Sum = 3 + 4 + 2 = 9, but heuristic caps at 8.
        let body = "x".repeat(500);
        assert_eq!(normalize_importance(None, &body, 2), 8);
    }

    #[test]
    fn normalize_importance_falls_back_to_heuristic_on_nan() {
        assert_eq!(normalize_importance(Some(f64::NAN), "", 0), 3);
    }
}
