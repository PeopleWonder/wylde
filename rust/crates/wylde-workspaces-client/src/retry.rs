//! Per-verb retry policy by verb shape (scope v2 §7.3).
//!
//! Idempotent reads back off exponentially up to N attempts; idempotent
//! writes get a single retry; non-idempotent and long-running verbs get
//! none. Each verb's policy lives in the [`crate::verbs`] table. Only
//! transport-level failures are retried — application errors short-circuit
//! immediately (see [`crate::error`]).

use std::time::Duration;

/// How a verb retries on transport failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// One attempt, no retry (non-idempotent / long-running verbs).
    NoRetry,
    /// Up to `max_attempts` total attempts, sleeping
    /// `initial_ms × 2^(attempt-1)` before each retry.
    ExponentialBackoff { max_attempts: u32, initial_ms: u64 },
}

impl RetryPolicy {
    /// Idempotent-read default: exp-backoff up to 4 attempts.
    pub const fn idempotent_read() -> Self {
        RetryPolicy::ExponentialBackoff {
            max_attempts: 4,
            initial_ms: 50,
        }
    }
    /// Idempotent-write default: a single retry (2 attempts total).
    pub const fn idempotent_write() -> Self {
        RetryPolicy::ExponentialBackoff {
            max_attempts: 2,
            initial_ms: 50,
        }
    }

    /// Total attempts this policy permits (always ≥ 1).
    pub fn max_attempts(&self) -> u32 {
        match self {
            RetryPolicy::NoRetry => 1,
            RetryPolicy::ExponentialBackoff { max_attempts, .. } => (*max_attempts).max(1),
        }
    }

    /// Delay to sleep BEFORE the retry following `attempt` (1-indexed).
    /// Returns `None` when no further attempt is allowed. The first attempt
    /// is always immediate; the delay before attempt k+1 is
    /// `initial_ms × 2^(k-1)`.
    pub fn backoff_delay(&self, attempt: u32) -> Option<Duration> {
        match self {
            RetryPolicy::NoRetry => None,
            RetryPolicy::ExponentialBackoff {
                max_attempts,
                initial_ms,
            } => {
                if attempt >= (*max_attempts).max(1) {
                    return None;
                }
                let shift = attempt.saturating_sub(1).min(16); // cap to avoid overflow
                Some(Duration::from_millis(
                    initial_ms.saturating_mul(1u64 << shift),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_retry_has_one_attempt() {
        assert_eq!(RetryPolicy::NoRetry.max_attempts(), 1);
        assert_eq!(RetryPolicy::NoRetry.backoff_delay(1), None);
    }

    #[test]
    fn exp_backoff_grows_then_stops() {
        let p = RetryPolicy::ExponentialBackoff {
            max_attempts: 3,
            initial_ms: 10,
        };
        assert_eq!(p.max_attempts(), 3);
        // before attempt 2: 10ms; before attempt 3: 20ms; no attempt 4.
        assert_eq!(p.backoff_delay(1), Some(Duration::from_millis(10)));
        assert_eq!(p.backoff_delay(2), Some(Duration::from_millis(20)));
        assert_eq!(p.backoff_delay(3), None);
    }

    #[test]
    fn max_attempts_floored_at_one() {
        let p = RetryPolicy::ExponentialBackoff {
            max_attempts: 0,
            initial_ms: 10,
        };
        assert_eq!(p.max_attempts(), 1);
    }
}
