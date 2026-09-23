//! Per-frame record of the controls [`crate::control`] constructed.
//!
//! Dev-only: this module does not exist without `test-support` (or inside this
//! crate's own unit tests), so the shipped Shell links no registry, no
//! thread-local, and no `Vec`.
//!
//! # Why a thread-local
//!
//! Same reason `wylde-gui-pipe`'s fake backend is one (`docs/gui-testing.md`):
//! `cargo test` runs a binary's tests in parallel, and a process-global would
//! let one panel's frame leak into another panel's assertions. gpui's
//! `TestDispatcher` polls every task — foreground *and* background — on the
//! thread that calls `run_until_parked`, so a panel's whole render happens on
//! the test's own thread. A thread-local is therefore both sufficient and
//! exactly scoped.
//!
//! This is the mirror of the choice made for `PipeNameOverride`, which *must*
//! be a process-global because the real transport connects on a tokio worker.
//! Rendering never leaves the test thread; transport does.

use std::cell::RefCell;

use gpui::SharedString;

thread_local! {
    /// Controls constructed since the last [`begin_frame`], in construction
    /// order, de-duplicated. A `Vec` rather than a set because order is part
    /// of the contract — a failing walk should print the same list every run.
    static CONSTRUCTED: RefCell<Vec<SharedString>> = const { RefCell::new(Vec::new()) };
}

/// Start recording a fresh frame, discarding the previous one.
///
/// Call this immediately before the draw whose controls you want to walk.
/// gpui clears its own `debug_bounds` map at the top of each frame; this keeps
/// the constructed-half aligned with the painted-half, so the two can be
/// intersected without one of them describing a stale frame.
pub fn begin_frame() {
    CONSTRUCTED.with(|c| c.borrow_mut().clear());
}

/// Record a control. Called by [`crate::control`]; not part of the public
/// contract for panel code, which should never touch the registry directly.
pub fn record(id: SharedString) {
    CONSTRUCTED.with(|c| {
        let mut c = c.borrow_mut();
        if !c.contains(&id) {
            c.push(id);
        }
    });
}

/// The controls constructed during the current frame, in construction order.
///
/// **Constructed is not painted.** An element built inside a section that was
/// never laid out appears here with no entry in gpui's `debug_bounds`. The
/// walk intersects the two rather than trusting this list alone — see the
/// crate docs.
pub fn constructed() -> Vec<SharedString> {
    CONSTRUCTED.with(|c| c.borrow().clone())
}
