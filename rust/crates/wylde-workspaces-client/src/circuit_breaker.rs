//! Per-verb circuit breaker (scope v2 §7.4).
//!
//! **5 consecutive failures → open for 30s.** While open, [`check`] returns
//! [`BreakerDecision::Open`] and the client fails fast into the consumer's
//! fallback instead of hanging. After the cooldown the breaker half-opens
//! and admits one probe; a success closes it, a failure re-opens it.
//!
//! The breaker is keyed per-verb so a flaky `graph` load doesn't trip
//! `ping`. It tracks completed *operations* (one record per call, after
//! retries are exhausted) — not individual retry attempts.
//!
//! [`check`]: CircuitBreaker::check

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default consecutive-failure threshold before the breaker opens.
pub const DEFAULT_THRESHOLD: u32 = 5;
/// Default cooldown the breaker stays open before admitting a probe.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// What the caller should do for a verb right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerDecision {
    /// Breaker closed — proceed normally.
    Closed,
    /// Breaker open — fail fast into the fallback; do not hit the pipe.
    Open,
    /// Cooldown elapsed — admit a single probe. Treated like `Closed` by the
    /// caller, but a failure here re-opens immediately.
    HalfOpen,
}

#[derive(Debug, Default, Clone)]
struct VerbState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

/// Per-pipe, per-verb circuit breaker. Cheap to clone-by-reference (hold it
/// behind an `Arc` or as a field on the client).
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    states: Mutex<HashMap<String, VerbState>>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    /// The spec default: 5 failures → 30s open.
    pub fn new() -> Self {
        Self::with_params(DEFAULT_THRESHOLD, DEFAULT_COOLDOWN)
    }

    /// Construct with explicit params (tests tune these low for speed).
    pub fn with_params(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Decide what to do for `verb` right now, transitioning open → half-open
    /// once the cooldown has elapsed.
    pub fn check(&self, verb: &str) -> BreakerDecision {
        let mut states = self.states.lock().expect("breaker poisoned");
        let st = states.entry(verb.to_string()).or_default();
        match st.opened_at {
            None => BreakerDecision::Closed,
            Some(opened) => {
                if opened.elapsed() >= self.cooldown {
                    BreakerDecision::HalfOpen
                } else {
                    BreakerDecision::Open
                }
            }
        }
    }

    /// Record a successful operation — resets the verb's failure count and
    /// closes the breaker.
    pub fn record_success(&self, verb: &str) {
        let mut states = self.states.lock().expect("breaker poisoned");
        let st = states.entry(verb.to_string()).or_default();
        st.consecutive_failures = 0;
        st.opened_at = None;
    }

    /// Record a failed operation. Opens (or re-opens, from half-open) the
    /// breaker once `threshold` consecutive failures accrue.
    pub fn record_failure(&self, verb: &str) {
        let mut states = self.states.lock().expect("breaker poisoned");
        let st = states.entry(verb.to_string()).or_default();

        // A failure while half-open (cooldown elapsed) immediately re-opens.
        let half_open = st
            .opened_at
            .map(|o| o.elapsed() >= self.cooldown)
            .unwrap_or(false);

        st.consecutive_failures = st.consecutive_failures.saturating_add(1);
        if half_open || st.consecutive_failures >= self.threshold {
            st.opened_at = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_until_threshold() {
        let cb = CircuitBreaker::with_params(5, Duration::from_secs(30));
        for _ in 0..4 {
            cb.record_failure("ping");
            assert_eq!(cb.check("ping"), BreakerDecision::Closed);
        }
        cb.record_failure("ping"); // 5th → open
        assert_eq!(cb.check("ping"), BreakerDecision::Open);
    }

    #[test]
    fn success_resets_failures() {
        let cb = CircuitBreaker::with_params(5, Duration::from_secs(30));
        for _ in 0..4 {
            cb.record_failure("ping");
        }
        cb.record_success("ping");
        for _ in 0..4 {
            cb.record_failure("ping");
            assert_eq!(cb.check("ping"), BreakerDecision::Closed);
        }
    }

    #[test]
    fn half_open_after_cooldown_then_close() {
        let cb = CircuitBreaker::with_params(2, Duration::from_millis(40));
        cb.record_failure("ping");
        cb.record_failure("ping");
        assert_eq!(cb.check("ping"), BreakerDecision::Open);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(cb.check("ping"), BreakerDecision::HalfOpen);
        cb.record_success("ping");
        assert_eq!(cb.check("ping"), BreakerDecision::Closed);
    }

    #[test]
    fn half_open_failure_reopens() {
        let cb = CircuitBreaker::with_params(2, Duration::from_millis(40));
        cb.record_failure("ping");
        cb.record_failure("ping");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(cb.check("ping"), BreakerDecision::HalfOpen);
        cb.record_failure("ping"); // probe fails → re-open
        assert_eq!(cb.check("ping"), BreakerDecision::Open);
    }

    #[test]
    fn breaker_is_per_verb() {
        let cb = CircuitBreaker::with_params(2, Duration::from_secs(30));
        cb.record_failure("graph");
        cb.record_failure("graph");
        assert_eq!(cb.check("graph"), BreakerDecision::Open);
        assert_eq!(cb.check("ping"), BreakerDecision::Closed);
    }
}
