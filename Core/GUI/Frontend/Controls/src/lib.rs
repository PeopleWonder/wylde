//! The one constructor every interactive Wylde GUI control routes through.
//!
//! # Why this exists
//!
//! The L7 panel-walk (#35) proves every panel *loads*. Nothing proved a
//! control in it *does anything* — a button could ship with an empty handler,
//! a stale handler, or no handler at all, and every gate stayed green, because
//! no test in the tree had ever clicked a GUI control through its real
//! listener. Issue #247 closes that.
//!
//! Clicking a control from a test needs two things the panel source does not
//! otherwise surface:
//!
//! 1. **an enumerable set of the controls that painted this frame** — not a
//!    hand-written list in the test, which is exactly the thing that goes
//!    quiet when someone adds a tenth button; and
//! 2. **the painted bounds of each**, so the click goes through gpui's real
//!    hit-testing rather than by calling the closure directly.
//!
//! [`control`] provides both, and costs the shipped Shell nothing.
//!
//! # The zero-cost claim, precisely
//!
//! In a release build `control(el, id)` compiles to `el.id(ElementId::Name(id))`
//! — the exact call site it replaces. Two things make that true:
//!
//! * the registry module is `#[cfg(any(test, feature = "test-support"))]`, and
//!   the feature is requested only from panels' `[dev-dependencies]`, which
//!   `resolver = "2"` never unifies into a normal lib build; and
//! * `debug_selector` is gpui's own method, and **gpui itself** compiles it as
//!   an `#[inline]` no-op that drops its closure argument unless gpui carries
//!   `test-support`. The closure is a ZST over a `SharedString` that is never
//!   invoked, so the `to_string()` inside it never runs.
//!
//! Verify the same way `docs/gui-testing.md` verifies the pipe seam:
//!
//! ```sh
//! cargo tree -p wylde-gui -e normal,features -i wylde-gui-controls  # → only "default"
//! ```
//!
//! # Why the registry is per-frame
//!
//! A control that is *constructed* is not necessarily a control that *paints*
//! — a row inside a collapsed section, or an off-screen item in a virtualized
//! list, is built and then never laid out. The walk must not click at bounds
//! that do not exist, and must not report a control as covered when the user
//! could never have reached it.
//!
//! So the two halves are recorded separately and intersected by the walk:
//!
//! | half | recorded by | when |
//! |---|---|---|
//! | *constructed* | [`registry`], from `control()` | during `render()` |
//! | *painted* | gpui's `debug_bounds`, from `debug_selector` | at prepaint |
//!
//! gpui clears `debug_bounds` at the top of every frame; [`registry::begin_frame`]
//! clears ours to match, so both describe the same frame.
//!
//! # Usage
//!
//! ```ignore
//! use wylde_gui_controls::control;
//!
//! // before:  div().id(ElementId::Name("tools-refresh".into())).on_mouse_down(..)
//! // after:
//! control(div(), "tools-refresh")
//!     .cursor_pointer()
//!     .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| { .. }))
//! ```
//!
//! The id must be **stable and unique within the frame**. Per-item controls
//! carry their item key (`format!("ext-toggle::{}", ext.name)`) — a duplicate
//! id would make two controls indistinguishable to both gpui's hit-testing and
//! the walk.

use gpui::{ElementId, InteractiveElement, SharedString, Stateful};

#[cfg(any(test, feature = "test-support"))]
pub mod registry;

#[cfg(any(test, feature = "test-support"))]
pub mod scan;

/// Give `el` a stable control id and make it interactive.
///
/// This is the **only** sanctioned way to build an interactive control in the
/// Wylde GUI; `wylde_check` rule 59 flags interactive sites that bypass it.
/// Routing through one constructor is what lets a test enumerate the controls
/// that painted instead of trusting a list someone has to remember to update.
///
/// In a shipped build this is `el.id(ElementId::Name(id.into()))` and nothing
/// else — see the module docs for why that is exact rather than approximate.
#[inline]
pub fn control<E: InteractiveElement>(el: E, id: impl Into<SharedString>) -> Stateful<E> {
    let id: SharedString = id.into();

    // Constructed-this-frame half. Compiled out entirely without the feature.
    #[cfg(any(test, feature = "test-support"))]
    registry::record(id.clone());

    // Painted-this-frame half. gpui stores the bounds under this key at
    // prepaint, and `VisualTestContext::debug_bounds` reads them back. Always
    // called: without gpui's `test-support` this is an `#[inline]` no-op that
    // never invokes the closure, so the `to_string()` never runs.
    let id_for_selector = id.clone();
    let el = el.debug_selector(move || id_for_selector.to_string());

    el.id(ElementId::Name(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::div;

    #[test]
    fn control_records_the_id_it_was_given() {
        registry::begin_frame();
        let _ = control(div(), "tools-refresh");
        assert_eq!(
            registry::constructed(),
            vec![SharedString::from("tools-refresh")]
        );
    }

    #[test]
    fn begin_frame_clears_the_previous_frames_controls() {
        registry::begin_frame();
        let _ = control(div(), "stale");
        registry::begin_frame();
        let _ = control(div(), "fresh");
        assert_eq!(
            registry::constructed(),
            vec![SharedString::from("fresh")],
            "a control from the previous frame must not be reported as rendered now"
        );
    }

    #[test]
    fn a_repeated_id_is_recorded_once() {
        // Two controls with the same id are indistinguishable to hit-testing.
        // The registry de-duplicates so the walk reports one uncovered control
        // rather than silently clicking the same bounds twice.
        registry::begin_frame();
        let _ = control(div(), "dup");
        let _ = control(div(), "dup");
        assert_eq!(registry::constructed().len(), 1);
    }

    #[test]
    fn ids_are_reported_in_construction_order() {
        registry::begin_frame();
        let _ = control(div(), "first");
        let _ = control(div(), "second");
        let _ = control(div(), "third");
        assert_eq!(
            registry::constructed(),
            vec![
                SharedString::from("first"),
                SharedString::from("second"),
                SharedString::from("third"),
            ],
            "stable order keeps a failing walk's output diffable run to run"
        );
    }
}
