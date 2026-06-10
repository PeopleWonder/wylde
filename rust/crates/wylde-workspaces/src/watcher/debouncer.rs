//! Edit debouncer — coalesce a burst of filesystem events into one delta per
//! path (Slice I).
//!
//! An editor save often fires several events in a few milliseconds (truncate,
//! write, rename-into-place, attribute touch). Re-ingesting on each would blow
//! the per-file budget and hammer the tree-sitter sidecar. The debouncer holds
//! each changed path until it has been quiet for the window (default 500ms),
//! then releases one collapsed change for it. Multiple files edited inside the
//! same window all come due together, so the watcher ingests them in a single
//! drained batch.
//!
//! Pure + clock-injected: every method that cares about time takes `now`
//! explicitly, so the coalescing logic is unit-tested deterministically with
//! `Instant` arithmetic — no sleeps, no wall clock. The async loop in
//! [`super`] supplies `Instant::now()`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The collapsed intent for a path after debouncing. The raw notify event set
/// (create / modify / rename / delete) folds down to just these two: a file
/// either still exists and needs (re)indexing, or it's gone and needs purging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Created or modified → re-extract + delta-upsert.
    Upsert,
    /// Deleted or renamed away → drop its graph + vector footprint.
    Remove,
}

/// One path's pending state: the latest collapsed kind + when it comes due.
#[derive(Clone, Copy, Debug)]
struct Pending {
    kind: ChangeKind,
    deadline: Instant,
}

/// Per-path debounce buffer.
#[derive(Debug)]
pub struct Debouncer {
    window: Duration,
    pending: HashMap<PathBuf, Pending>,
}

impl Debouncer {
    /// New debouncer with the given quiet-window.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
        }
    }

    /// The configured quiet-window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Record a raw change for `path` observed at `now`. The newest event wins
    /// the kind (a delete after edits → Remove; a re-create after a delete →
    /// Upsert), and the deadline is pushed to `now + window` so a continuing
    /// burst keeps coalescing instead of releasing mid-save.
    pub fn record(&mut self, path: PathBuf, kind: ChangeKind, now: Instant) {
        self.pending.insert(
            path,
            Pending {
                kind,
                deadline: now + self.window,
            },
        );
    }

    /// Nothing pending?
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Count of distinct paths currently buffered.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// The earliest pending deadline, if any — what the loop sleeps until.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.deadline).min()
    }

    /// Remove and return every path whose quiet-window has elapsed by `now`.
    /// Order is unspecified (it's a map drain).
    pub fn drain_due(&mut self, now: Instant) -> Vec<(PathBuf, ChangeKind)> {
        let due: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        due.into_iter()
            .map(|k| {
                let p = self.pending.remove(&k).expect("just-collected key");
                (k, p.kind)
            })
            .collect()
    }

    /// Drop everything without releasing it (used when pausing — a resume
    /// re-walks the whole workspace, so buffered deltas are redundant).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn ten_rapid_edits_collapse_to_one_due_change() {
        let win = Duration::from_millis(500);
        let mut d = Debouncer::new(win);
        let start = t0();
        let path = PathBuf::from("/proj/src/main.rs");
        // 10 edits, each 10ms apart — all inside one continuing burst.
        for i in 0..10 {
            d.record(
                path.clone(),
                ChangeKind::Upsert,
                start + Duration::from_millis(i * 10),
            );
        }
        assert_eq!(d.len(), 1, "one path buffered, not ten");

        let last_edit = start + Duration::from_millis(90);
        // Just before the window elapses from the LAST edit: nothing due.
        assert!(d
            .drain_due(last_edit + Duration::from_millis(400))
            .is_empty());
        // After the window from the last edit: exactly one collapsed change.
        let due = d.drain_due(last_edit + win + Duration::from_millis(1));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, path);
        assert_eq!(due[0].1, ChangeKind::Upsert);
        assert!(d.is_empty(), "drained");
    }

    #[test]
    fn multiple_files_in_one_window_batch_together() {
        let win = Duration::from_millis(500);
        let mut d = Debouncer::new(win);
        let start = t0();
        d.record(PathBuf::from("/a.rs"), ChangeKind::Upsert, start);
        d.record(
            PathBuf::from("/b.rs"),
            ChangeKind::Upsert,
            start + Duration::from_millis(20),
        );
        d.record(
            PathBuf::from("/c.rs"),
            ChangeKind::Remove,
            start + Duration::from_millis(40),
        );

        let due = d.drain_due(start + Duration::from_millis(40) + win);
        let paths: std::collections::HashSet<PathBuf> =
            due.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(paths.len(), 3, "all three released in one batch");
        assert!(d.is_empty());
    }

    #[test]
    fn latest_event_kind_wins() {
        let win = Duration::from_millis(100);
        let mut d = Debouncer::new(win);
        let start = t0();
        let p = PathBuf::from("/x.rs");
        // modify → modify → delete: net Remove.
        d.record(p.clone(), ChangeKind::Upsert, start);
        d.record(
            p.clone(),
            ChangeKind::Upsert,
            start + Duration::from_millis(5),
        );
        d.record(
            p.clone(),
            ChangeKind::Remove,
            start + Duration::from_millis(10),
        );
        let due = d.drain_due(start + Duration::from_millis(10) + win);
        assert_eq!(due, vec![(p.clone(), ChangeKind::Remove)]);

        // delete → re-create: net Upsert.
        d.record(p.clone(), ChangeKind::Remove, start);
        d.record(
            p.clone(),
            ChangeKind::Upsert,
            start + Duration::from_millis(5),
        );
        let due = d.drain_due(start + Duration::from_millis(5) + win);
        assert_eq!(due, vec![(p, ChangeKind::Upsert)]);
    }

    #[test]
    fn next_deadline_tracks_earliest_pending() {
        let win = Duration::from_millis(500);
        let mut d = Debouncer::new(win);
        assert!(d.next_deadline().is_none());
        let start = t0();
        d.record(PathBuf::from("/a"), ChangeKind::Upsert, start);
        d.record(
            PathBuf::from("/b"),
            ChangeKind::Upsert,
            start + Duration::from_millis(30),
        );
        // Earliest deadline is /a's (start + window).
        assert_eq!(d.next_deadline(), Some(start + win));
    }

    #[test]
    fn partial_drain_leaves_not_yet_due() {
        let win = Duration::from_millis(100);
        let mut d = Debouncer::new(win);
        let start = t0();
        d.record(PathBuf::from("/early"), ChangeKind::Upsert, start);
        d.record(
            PathBuf::from("/late"),
            ChangeKind::Upsert,
            start + Duration::from_millis(80),
        );
        // At start+window only /early is due; /late waits.
        let due = d.drain_due(start + win);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, PathBuf::from("/early"));
        assert_eq!(d.len(), 1, "/late still buffered");
    }

    #[test]
    fn clear_drops_everything() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(PathBuf::from("/a"), ChangeKind::Upsert, t0());
        assert!(!d.is_empty());
        d.clear();
        assert!(d.is_empty());
    }
}
