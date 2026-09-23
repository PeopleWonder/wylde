//! Thin adapter over the `rand` crate — the single place in the workspace that
//! touches `rand` directly, so a future breaking `rand` bump is a one-file
//! change rather than a workspace-wide sweep (see #290, dependency isolation).
//!
//! Only the surface Wylde actually uses is exposed: filling a byte buffer, a
//! fixed-size random byte array, and a bounded index. Everything draws from
//! `rand`'s thread-local generator, which is seeded from the OS CSPRNG on first
//! use — suitable for the unguessable pairing codes / ids the callers mint.
//!
//! `rand` is a 0.x crate that breaks freely: the 0.8→0.9 bump renamed
//! `thread_rng`→`rng` and `gen_range`→`random_range` (migrated here in one
//! edit, #290). When it next breaks, this module remains the only edit site.

use rand::{Rng, RngCore};

/// Fill `buf` with random bytes from the thread-local CSPRNG-seeded generator.
pub fn fill_bytes(buf: &mut [u8]) {
    rand::rng().fill_bytes(buf);
}

/// Return `N` random bytes.
///
/// Convenience over [`fill_bytes`] for the common "short random suffix" case.
pub fn byte_array<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    fill_bytes(&mut buf);
    buf
}

/// Uniform random index in `0..len` (half-open), for choosing an element of a
/// slice of length `len`.
///
/// # Panics
/// Panics if `len == 0` — an empty range has no valid index. This mirrors the
/// behaviour of the underlying `random_range(0..len)` on an empty range, so the
/// contract is unchanged from the direct call sites this replaced.
pub fn index_below(len: usize) -> usize {
    rand::rng().random_range(0..len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_below_one_is_always_zero() {
        for _ in 0..256 {
            assert_eq!(index_below(1), 0);
        }
    }

    #[test]
    fn index_below_stays_in_range() {
        let len = 6;
        for _ in 0..10_000 {
            assert!(index_below(len) < len);
        }
    }

    #[test]
    fn index_below_covers_the_whole_range() {
        // Probabilistic: over many draws every index in 0..len should appear.
        // With 10k draws over 6 buckets the chance any bucket is missed is
        // astronomically small, so a miss means a real bug (e.g. an off-by-one
        // upper bound), not flakiness.
        let len = 6;
        let mut seen = [false; 6];
        for _ in 0..10_000 {
            seen[index_below(len)] = true;
        }
        assert!(
            seen.iter().all(|&s| s),
            "every index in 0..{len} should appear"
        );
    }

    #[test]
    fn byte_array_has_requested_length() {
        let a = byte_array::<3>();
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn fill_bytes_fills_the_whole_buffer() {
        let mut buf = [0u8; 16];
        fill_bytes(&mut buf);
        // Not a randomness-quality test — just that the call touches the buffer.
        // An all-zero 16-byte draw is a 1-in-2^128 event, so treat it as a bug.
        assert!(
            buf.iter().any(|&b| b != 0),
            "buffer should not remain all-zero"
        );
    }

    #[test]
    fn zero_length_fill_is_a_noop() {
        let mut empty: [u8; 0] = [];
        fill_bytes(&mut empty); // must not panic
    }
}
