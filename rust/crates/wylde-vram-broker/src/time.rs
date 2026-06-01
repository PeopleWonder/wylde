//! Time helper — wall-clock epoch seconds as `f64`.
//!
//! The Python broker serializes timestamps as Python `time.time()` floats
//! (Unix epoch seconds, fractional). For wire compatibility this module
//! exposes one helper that produces values in the same domain. Kept as its
//! own (tiny) module so callers don't reach into `std::time` ad-hoc.

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock seconds since Unix epoch as `f64`. Matches Python `time.time()`.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        // Pre-epoch clock would be very unusual; falling back to 0 keeps the
        // function infallible without forcing every caller into a Result.
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_recent_epoch() {
        let t = now_secs();
        // After 2025-01-01.
        assert!(t > 1_700_000_000.0);
    }
}
