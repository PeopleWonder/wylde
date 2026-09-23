//! Cross-panel navigation bus.
//!
//! Panels frequently want to direct the user to a sibling — the
//! Dashboard's service-health rows jump to Tools, recent-activity rows
//! jump to Chat or Memory.  The Shell owns selection state (`NavModel`
//! lives in the Shell crate) but the panel crates don't depend on the
//! Shell — that would invert the panels-feed-the-shell dep graph.
//!
//! This module is the narrow channel between them:
//!
//!   * The Shell calls [`install_sender`] once during startup with the
//!     tx half of `tokio::sync::mpsc::UnboundedChannel<String>`.
//!   * Panels call [`request_nav`] with a registry key like
//!     `"core/tools"`; the request lands on the rx half the Shell is
//!     draining inside its gpui task.
//!   * If no Shell installed a sender (unit tests, headless harness)
//!     `request_nav` returns `false` so the caller can fall back to a
//!     no-op or a toast.
//!
//! Lives in `wylde-gui-pipe` rather than `wylde-panel-registry` to
//! avoid a registry-deps-on-panel-deps-on-registry cycle (the registry
//! depends on every panel crate; panels depend on the pipe crate).

use std::sync::OnceLock;

use tokio::sync::mpsc;

static SENDER: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();

/// Install the Shell-owned sender.  Returns `false` on the second
/// install (the cell is already initialised); the live binary calls
/// this once, so the second-call path exists for tests that want to
/// assert install idempotency.
pub fn install_nav_sender(tx: mpsc::UnboundedSender<String>) -> bool {
    SENDER.set(tx).is_ok()
}

/// Ask the Shell to switch to the panel keyed `key`.  Returns `false`
/// when no Shell is listening or the channel was dropped (the request
/// is silently absorbed) so the caller can fall back gracefully.
pub fn request_nav(key: &str) -> bool {
    // Dev-only observation seam (#247). A nav request is a real, observable
    // effect of clicking a control — the Dashboard's service chips and
    // empty-state rows do nothing else — but it is neither a backend call nor
    // a change to the panel's own state, so a control walk cannot see it
    // otherwise. Recording it here gives the walk a third channel.
    //
    // A THREAD-LOCAL, not a reader on `SENDER`: `SENDER` is a process-wide
    // `OnceLock`, so a test that installed a real channel would collect nav
    // requests from every other test running in parallel in the same binary.
    // Contamination there could only turn a dead control into a live-looking
    // one — the wrong direction for a gate to be wrong in. Same reasoning, and
    // the same shape, as the scripted backend's thread-local (see
    // `docs/gui-testing.md`).
    #[cfg(feature = "test-support")]
    nav_probe::record(key);

    let Some(tx) = SENDER.get() else {
        return false;
    };
    tx.send(key.to_owned()).is_ok()
}

/// Dev-only record of the nav requests made on this thread.
///
/// Compiled out entirely without `test-support`, which is requested only from
/// the panels' `[dev-dependencies]` — so the shipped Shell has no probe, no
/// thread-local, and no recording branch inside [`request_nav`].
#[cfg(feature = "test-support")]
pub mod nav_probe {
    use std::cell::RefCell;

    thread_local! {
        static NAV: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record(key: &str) {
        NAV.with(|n| n.borrow_mut().push(key.to_owned()));
    }

    /// Every nav request made on this thread so far, in order.
    pub fn requests() -> Vec<String> {
        NAV.with(|n| n.borrow().clone())
    }

    /// How many nav requests have been made on this thread.
    pub fn count() -> usize {
        NAV.with(|n| n.borrow().len())
    }

    /// Forget them — call between independent phases of a test.
    pub fn clear() {
        NAV.with(|n| n.borrow_mut().clear());
    }
}

/// Whether a sender has been installed.  Useful for diagnostics + a
/// "is the Shell wired up?" smoke assertion in tests.
pub fn is_nav_installed() -> bool {
    SENDER.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    // OnceLock is process-wide; we can't reset it between tests, so
    // these assertions are weak (must-not-panic + post-install state).
    // The "panel calls request_nav, Shell drains the rx" round trip
    // lives in the Shell's integration tests where the install order
    // is deterministic.

    #[test]
    fn request_nav_does_not_panic_without_sender() {
        let _ = request_nav("core/dashboard");
    }

    #[test]
    fn install_sender_post_state_is_installed() {
        let (tx, _rx) = mpsc::unbounded_channel();
        // Either we win the race (true) or another test won (false);
        // both leave the cell initialised.
        let _ = install_nav_sender(tx);
        assert!(is_nav_installed());
    }
}
