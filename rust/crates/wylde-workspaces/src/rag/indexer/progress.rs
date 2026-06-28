//! Index-progress model + rate/ETA math — the pure core behind the GUI's
//! live "Indexing…" → progress-bar+ETA affordance.
//!
//! The indexer ([`super`]) already enumerates every file up front (the walk)
//! and chunks them before a single vector is embedded, so the **total** chunk
//! count is known the moment the (slow) embed phase begins. That makes a real
//! determinate progress bar + ETA possible: during the walk we have no total
//! yet (indeterminate), and once chunking is done we switch to a percent + an
//! `remaining ÷ rolling-rate` ETA.
//!
//! Everything here is pure and timestamp-driven (seconds as `f64`), so the
//! rate/ETA logic is unit-tested without a clock: the live [`super::mod`]
//! reporter feeds it `Instant::elapsed().as_secs_f64()`. The snapshot
//! [`IndexProgress`] rides the existing `RagState` → `list_mru` channel the
//! GUI already polls — no parallel progress channel is invented.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Which stage of the index pass is running. Surfaced to the GUI so the
/// status reads "Walking…", "Embedding…", etc. [`Phase::Walk`] / [`Phase::Chunk`]
/// are indeterminate (no total yet); [`Phase::Embed`] / [`Phase::Persist`] are
/// determinate (totals known) — see [`Phase::is_determinate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Enumerating + chunking files (full reindex). Total not yet known.
    Walk,
    /// Chunking changed/new files (delta reindex). Total not yet known.
    Chunk,
    /// Embedding chunks — the long, paced phase. Total known ⇒ ETA possible.
    Embed,
    /// Writing chunks + manifest to disk. Brief, determinate.
    Persist,
}

impl Phase {
    /// True once the total work is countable (a real percent + ETA are
    /// meaningful). The walk/chunk phases run before the total is known, so
    /// the GUI shows an indeterminate state for them.
    pub fn is_determinate(self) -> bool {
        matches!(self, Phase::Embed | Phase::Persist)
    }

    /// Short human label for the GUI status line.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Walk => "Scanning files",
            Phase::Chunk => "Reading changes",
            Phase::Embed => "Embedding",
            Phase::Persist => "Saving",
        }
    }
}

/// A live snapshot of index progress, serialized into `RagState` and joined
/// onto each `list_mru` row so the GUI can render percent + bar + "X / Y
/// files" + ETA. Additive + `Option`-wrapped on `RagState`, so older readers
/// (and the not-indexing case) are unaffected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexProgress {
    /// Current pipeline stage.
    pub phase: Phase,
    /// `false` while the total is still unknown (walk/chunk) ⇒ the GUI shows
    /// an indeterminate bar; `true` once counting is done ⇒ percent + ETA.
    pub determinate: bool,
    /// Distinct files embedded so far (the file currently in flight counts).
    pub files_done: u32,
    /// Distinct files to embed this pass (0 until known).
    pub files_total: u32,
    /// Chunks embedded so far.
    pub chunks_done: u32,
    /// Chunks to embed this pass (0 until known) — drives the percent + ETA.
    pub chunks_total: u32,
    /// Rolling embed throughput, chunks/sec (0 before two samples exist).
    pub items_per_sec: f64,
    /// Estimated seconds remaining (`remaining ÷ rate`), or `None` when the
    /// total or rate isn't known yet (so the GUI shows "—" not a bogus ETA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<f64>,
}

impl IndexProgress {
    /// An indeterminate snapshot for the walk/chunk phase: no total, no ETA.
    pub fn indeterminate(phase: Phase) -> Self {
        IndexProgress {
            phase,
            determinate: false,
            files_done: 0,
            files_total: 0,
            chunks_done: 0,
            chunks_total: 0,
            items_per_sec: 0.0,
            eta_secs: None,
        }
    }

    /// Fraction complete in `0.0..=1.0`, or `None` when the total isn't known
    /// yet (indeterminate). Guards divide-by-zero: a zero total ⇒ `None`.
    pub fn ratio(&self) -> Option<f64> {
        if !self.determinate || self.chunks_total == 0 {
            return None;
        }
        let r = self.chunks_done as f64 / self.chunks_total as f64;
        Some(r.clamp(0.0, 1.0))
    }

    /// Whole-percent complete (0..=100), or `None` when indeterminate.
    pub fn percent(&self) -> Option<u32> {
        self.ratio().map(|r| (r * 100.0).round() as u32)
    }
}

/// Number of distinct files touched within the first `done` chunks, given a
/// per-chunk file ordinal (`chunk_file_idx[i]` = 0-based ordinal of the file
/// owning chunk `i`). The walk yields a file's chunks consecutively, so the
/// distinct-file count over a prefix is just `last_ordinal + 1` — the file
/// currently being embedded counts as "in flight". Monotonic in `done`.
pub fn files_done_for(chunk_file_idx: &[u32], done: usize) -> u32 {
    if done == 0 || chunk_file_idx.is_empty() {
        return 0;
    }
    let i = done.min(chunk_file_idx.len()) - 1;
    chunk_file_idx[i] + 1
}

/// Rolling-window throughput tracker. Holds `(timestamp_secs, cumulative_done)`
/// samples within a trailing window and derives chunks/sec from the window's
/// span — so a transient stall or a fast burst doesn't whipsaw the ETA the way
/// an instantaneous (last-two-samples) rate would.
///
/// Timestamp-driven (not `Instant`-driven) so the math is unit-testable.
#[derive(Clone, Debug)]
pub struct RateTracker {
    window_secs: f64,
    samples: VecDeque<(f64, u64)>,
}

impl RateTracker {
    /// A tracker averaging over a trailing `window_secs` window. A non-finite
    /// or non-positive window falls back to 1s so the math never divides by a
    /// degenerate span.
    pub fn new(window_secs: f64) -> Self {
        let window_secs = if window_secs.is_finite() && window_secs > 0.0 {
            window_secs
        } else {
            1.0
        };
        RateTracker {
            window_secs,
            samples: VecDeque::new(),
        }
    }

    /// Record a cumulative-progress sample at time `t` (seconds). Out-of-order
    /// or non-finite timestamps are ignored so a clock hiccup can't corrupt the
    /// window. Evicts samples older than the window, always keeping at least the
    /// two needed to derive a rate.
    pub fn observe(&mut self, t: f64, done: u64) {
        if !t.is_finite() {
            return;
        }
        if let Some(&(last_t, _)) = self.samples.back() {
            if t < last_t {
                return;
            }
        }
        self.samples.push_back((t, done));
        while self.samples.len() > 2 {
            match self.samples.front() {
                Some(&(front_t, _)) if t - front_t > self.window_secs => {
                    self.samples.pop_front();
                }
                _ => break,
            }
        }
    }

    /// Chunks/sec across the retained window. `0.0` until two samples exist or
    /// when the window span is zero (guards divide-by-zero before the total /
    /// any elapsed time is known).
    pub fn rate(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let (t0, d0) = *self.samples.front().unwrap();
        let (t1, d1) = *self.samples.back().unwrap();
        let dt = t1 - t0;
        if dt <= 0.0 {
            return 0.0;
        }
        d1.saturating_sub(d0) as f64 / dt
    }

    /// Seconds remaining to finish `total` from `done` at the current rolling
    /// rate. `None` when the total is unknown (`0`) or the rate is not yet
    /// positive — so the caller shows "—" rather than an infinite/bogus ETA.
    pub fn eta_secs(&self, done: u64, total: u64) -> Option<f64> {
        eta_from_rate(done, total, self.rate())
    }
}

/// Pure ETA: `remaining ÷ rate`, or `None` when `total` is unknown (`0`),
/// already complete, or the rate is non-positive / non-finite. Kept free of
/// the tracker so the divide-by-zero + monotonicity guarantees are tested in
/// isolation.
pub fn eta_from_rate(done: u64, total: u64, rate: f64) -> Option<f64> {
    if total == 0 || !rate.is_finite() || rate <= 0.0 {
        return None;
    }
    let remaining = total.saturating_sub(done);
    if remaining == 0 {
        return Some(0.0);
    }
    Some(remaining as f64 / rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_determinacy_and_labels() {
        assert!(!Phase::Walk.is_determinate());
        assert!(!Phase::Chunk.is_determinate());
        assert!(Phase::Embed.is_determinate());
        assert!(Phase::Persist.is_determinate());
        assert_eq!(Phase::Embed.label(), "Embedding");
    }

    #[test]
    fn phase_serializes_snake_case() {
        // The GUI parses these strings — pin the wire form.
        assert_eq!(serde_json::to_value(Phase::Walk).unwrap(), "walk");
        assert_eq!(serde_json::to_value(Phase::Embed).unwrap(), "embed");
        assert_eq!(serde_json::to_value(Phase::Persist).unwrap(), "persist");
    }

    #[test]
    fn indeterminate_has_no_total_no_eta() {
        let p = IndexProgress::indeterminate(Phase::Walk);
        assert!(!p.determinate);
        assert_eq!(p.ratio(), None, "no ratio before the total is known");
        assert_eq!(p.percent(), None);
        assert_eq!(p.eta_secs, None);
    }

    #[test]
    fn determinate_to_indeterminate_transition() {
        // The load-bearing transition: walk (indeterminate) → embed
        // (determinate). Before the switch there's no percent; after it there is.
        let walk = IndexProgress::indeterminate(Phase::Walk);
        assert_eq!(walk.percent(), None);

        let embed = IndexProgress {
            phase: Phase::Embed,
            determinate: true,
            files_done: 1,
            files_total: 10,
            chunks_done: 25,
            chunks_total: 100,
            items_per_sec: 5.0,
            eta_secs: Some(15.0),
        };
        assert_eq!(embed.percent(), Some(25));
        assert_eq!(embed.ratio(), Some(0.25));
    }

    #[test]
    fn ratio_guards_zero_total() {
        // A determinate snapshot whose total is somehow zero must NOT divide by
        // zero — it returns None, not NaN/inf.
        let p = IndexProgress {
            phase: Phase::Embed,
            determinate: true,
            chunks_done: 0,
            chunks_total: 0,
            ..IndexProgress::indeterminate(Phase::Embed)
        };
        assert_eq!(p.ratio(), None);
        assert_eq!(p.percent(), None);
    }

    #[test]
    fn ratio_clamps_overshoot() {
        let p = IndexProgress {
            phase: Phase::Embed,
            determinate: true,
            chunks_done: 120,
            chunks_total: 100,
            ..IndexProgress::indeterminate(Phase::Embed)
        };
        assert_eq!(p.ratio(), Some(1.0), "never exceeds 100%");
        assert_eq!(p.percent(), Some(100));
    }

    #[test]
    fn files_done_counts_in_flight_file() {
        // chunk_file_idx: file0 owns chunks 0,1; file1 owns 2; file2 owns 3,4.
        let idx = [0u32, 0, 1, 2, 2];
        assert_eq!(files_done_for(&idx, 0), 0, "nothing started");
        assert_eq!(files_done_for(&idx, 1), 1, "in file0");
        assert_eq!(files_done_for(&idx, 2), 1, "still file0 (its 2nd chunk)");
        assert_eq!(files_done_for(&idx, 3), 2, "now in file1");
        assert_eq!(files_done_for(&idx, 5), 3, "all three files touched");
        // Defensive: done past the end saturates, never panics.
        assert_eq!(files_done_for(&idx, 99), 3);
        assert_eq!(files_done_for(&[], 5), 0);
    }

    #[test]
    fn files_done_is_monotonic() {
        let idx: Vec<u32> = (0..50u32).flat_map(|f| [f, f]).collect();
        let mut prev = 0;
        for done in 0..=idx.len() {
            let now = files_done_for(&idx, done);
            assert!(now >= prev, "files_done must never decrease");
            prev = now;
        }
    }

    #[test]
    fn rate_is_zero_before_two_samples() {
        let mut t = RateTracker::new(10.0);
        assert_eq!(t.rate(), 0.0, "no samples");
        t.observe(0.0, 0);
        assert_eq!(t.rate(), 0.0, "one sample — no span yet, no divide-by-zero");
    }

    #[test]
    fn rate_over_window() {
        let mut t = RateTracker::new(10.0);
        t.observe(0.0, 0);
        t.observe(2.0, 20); // 20 chunks in 2s
        assert!((t.rate() - 10.0).abs() < 1e-9, "10 chunks/sec");
    }

    #[test]
    fn rate_ignores_zero_span_and_backwards_time() {
        let mut t = RateTracker::new(10.0);
        t.observe(1.0, 5);
        t.observe(1.0, 9); // same timestamp ⇒ zero span ⇒ no divide-by-zero
        assert_eq!(t.rate(), 0.0);
        // A backwards timestamp is dropped, leaving the zero-span pair.
        t.observe(0.5, 99);
        assert_eq!(t.rate(), 0.0);
    }

    #[test]
    fn rate_evicts_stale_samples_but_keeps_two() {
        let mut t = RateTracker::new(5.0);
        t.observe(0.0, 0);
        t.observe(1.0, 10);
        // Jump well past the window — old samples evicted, recent rate used.
        t.observe(20.0, 110); // +100 chunks over 19s from t=1 → but window keeps last two
        t.observe(22.0, 130); // +20 over 2s = 10/s
        let r = t.rate();
        assert!(r > 0.0, "still derives a rate after eviction");
        // Window kept only recent samples, so the rate reflects ~10/s, not the
        // long-run average.
        assert!((r - 10.0).abs() < 2.0, "rate ~10/s from the recent window, got {r}");
    }

    #[test]
    fn eta_unknown_total_is_none() {
        assert_eq!(eta_from_rate(0, 0, 5.0), None, "no total ⇒ no ETA");
    }

    #[test]
    fn eta_zero_rate_is_none() {
        assert_eq!(eta_from_rate(0, 100, 0.0), None);
        assert_eq!(eta_from_rate(0, 100, -1.0), None);
        assert_eq!(eta_from_rate(0, 100, f64::NAN), None);
        assert_eq!(eta_from_rate(0, 100, f64::INFINITY), None);
    }

    #[test]
    fn eta_basic_and_complete() {
        // 100 remaining at 10/s ⇒ 10s.
        assert_eq!(eta_from_rate(0, 100, 10.0), Some(10.0));
        // 50 done of 100 at 10/s ⇒ 5s.
        assert_eq!(eta_from_rate(50, 100, 10.0), Some(5.0));
        // Done ⇒ 0s, never negative.
        assert_eq!(eta_from_rate(100, 100, 10.0), Some(0.0));
        assert_eq!(eta_from_rate(150, 100, 10.0), Some(0.0), "overshoot saturates");
    }

    #[test]
    fn eta_decreases_monotonically_at_steady_rate() {
        // Feed a steady 10 chunks/sec and assert the ETA only ever shrinks.
        let total = 1000u64;
        let mut tracker = RateTracker::new(30.0);
        let mut prev: Option<f64> = None;
        for step in 0..=20u64 {
            let t = step as f64; // 1s ticks
            let done = step * 10; // 10 chunks/sec
            tracker.observe(t, done);
            if let Some(eta) = tracker.eta_secs(done, total) {
                if let Some(p) = prev {
                    assert!(eta <= p + 1e-9, "ETA must be monotonically non-increasing: {eta} > {p}");
                }
                prev = Some(eta);
            }
        }
        assert!(prev.is_some(), "an ETA was produced once the rate was known");
    }
}
