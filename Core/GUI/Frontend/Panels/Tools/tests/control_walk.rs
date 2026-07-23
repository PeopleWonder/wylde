//! L7 **control**-walk — Tools (issue #247, pilot panel).
//!
//! The panel-walk next door (`panel_walk.rs`) proves this panel *loads*.
//! Nothing proved a control in it *does anything*. This does: it draws the
//! real panel, enumerates the controls that actually painted, clicks each one
//! through gpui's real hit-testing and the real listener, and asserts an
//! **observable effect** followed.
//!
//! # The three failures this catches that panel-walk cannot
//!
//! 1. **Dead handler** — the closure compiles and does nothing. No effect
//!    after the click → red.
//! 2. **Unwired control** — a cursor-pointer element with no listener at all.
//!    Same signature: no effect → red.
//! 3. **Panic on click** — the listener drives the panel into a branch
//!    (loaded / error) that panel-walk never repaints. The click repaints it,
//!    so the panic surfaces here rather than in front of the user.
//!
//! # The oracle
//!
//! "Something observable happened" is deliberately *not* per-control expected
//! behaviour — that would be a behavioural test per button, which is the cost
//! that stops this from scaling to the 140 sites the rest of the tree holds.
//! It is a **delta** over two cheap, already-existing observations:
//!
//! * **backend** — `ScriptedBackend` records every call (`docs/gui-testing.md`);
//!   a click that fires IPC moves the count.
//! * **state** — a per-panel `fingerprint` closure (here `tools_fingerprint`)
//!   over the fields the panel's own panel-walk already asserts on. One closure
//!   per panel, not one per control.
//!
//! A control passes if **either** moved. That is a weak assertion per control
//! and a strong one in aggregate: it cannot tell you the button did the *right*
//! thing, but it cannot be satisfied by a button that does *nothing* — which is
//! exactly the class #247 is about. Panels that want per-control behavioural
//! depth keep writing ordinary windowed tests next to this one.
//!
//! Nav and modal effects fold into the same two channels: nav publishes on a
//! bus the panel reads back (state), and a modal is panel state by definition.
//!
//! # Known gap (tracked on #247, part 2)
//!
//! A control that only paints once a **modal is open** is not in the first
//! frame's registry, so this walk never reaches it. Covering those needs a
//! per-panel "open the modal, walk again" pre-step. Tools has no modal, which
//! is part of why it is the pilot.

use gpui::{px, Modifiers, TestAppContext, VisualTestContext};
use serde_json::json;

use wylde_gui_controls::registry;
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_tools::ToolsPanel;

// ── The walk harness ─────────────────────────────────────────────────
//
// Inline in the pilot panel on purpose. It is extracted to a shared crate in
// #247 part 2, once a second and third panel have shown which parts are
// genuinely common — extracting from one example would be guessing.

// ── A trap worth knowing about before you copy this file ─────────────
//
// **Mount with `add_window`, not `open_window`.** At gpui rev `b3d93d44`,
// `TestAppContext::open_window(size, …)` sets the window's reported
// `viewport_size` but the root element still lays out against the test
// *display* (1920×1080). Every control then paints at coordinates outside the
// window you asked for, `simulate_click` at those coordinates hits nothing,
// and every control in the walk reads as dead — a total false positive that
// looks exactly like the bug this test is for.
//
// `add_window` maximizes to the test display, so layout and viewport agree and
// clicks land. The test display is a fixed size in gpui's `TestPlatform`, so
// this is deterministic, not machine-dependent.
//
// (A `simulate_mouse_move` before each click was measured against this panel
// and changes nothing — `dispatch_mouse_event` recomputes the hit test from
// the mouse-down position itself. It is omitted so the walk sends exactly the
// events it claims to.)

/// What a click is allowed to change, sampled either side of the click.
#[derive(Debug, PartialEq, Eq)]
struct Effect {
    /// Total backend calls recorded so far.
    backend_calls: usize,
    /// Panel-supplied state fingerprint.
    state: String,
}

/// One walked control and what its click did.
struct Walked {
    id: String,
    painted: bool,
    before: Effect,
    after: Effect,
}

impl Walked {
    /// The oracle. A control "did something" if either channel moved.
    fn had_effect(&self) -> bool {
        self.before != self.after
    }
}

/// Draw the panel, then click every control that painted.
///
/// `fingerprint` is the panel's state snapshot — one closure per panel.
fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<ToolsPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
    fingerprint: impl Fn(&ToolsPanel) -> String,
) -> Vec<Walked> {
    let mut vcx = VisualTestContext::from_window(window.into(), cx);

    // Start a fresh frame and force the root view to re-render into it. gpui
    // clears its own `debug_bounds` at the top of every real frame, so after
    // this the constructed-half and the painted-half describe the same tree.
    registry::begin_frame();
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();

    let ids = registry::constructed();
    assert!(
        !ids.is_empty(),
        "the registry is empty — either this panel routes no control through \
         `controls::control()`, or the draw never happened. An empty walk is a \
         disarmed gate, not a pass."
    );

    let mut walked = Vec::new();
    for id in ids {
        // `debug_bounds` is keyed by `&'static str`. Control ids are a small
        // bounded set per test binary, so leaking each once is cheaper than
        // threading a lifetime through the registry.
        let key: &'static str = Box::leak(id.to_string().into_boxed_str());
        let Some(bounds) = vcx.debug_bounds(key) else {
            // Constructed but never laid out — inside a collapsed section, or
            // off the bottom of the viewport. Recorded, not clicked: there is
            // no place on screen for the user to click either.
            walked.push(Walked {
                id: id.to_string(),
                painted: false,
                before: Effect {
                    backend_calls: 0,
                    state: String::new(),
                },
                after: Effect {
                    backend_calls: 0,
                    state: String::new(),
                },
            });
            continue;
        };

        let snap = |vcx: &mut VisualTestContext| Effect {
            backend_calls: fake.calls().len(),
            state: window
                .update(vcx, |panel, _w, _cx| fingerprint(panel))
                .unwrap(),
        };

        let before = snap(&mut vcx);
        // The real thing: a platform mouse event at the control's painted
        // centre, routed through gpui hit-testing to whatever listener the
        // panel actually attached. If the listener panics, the test dies here
        // — which is the correct outcome.
        vcx.simulate_click(bounds.center(), Modifiers::none());
        vcx.run_until_parked();
        let after = snap(&mut vcx);

        walked.push(Walked {
            id: id.to_string(),
            painted: true,
            before,
            after,
        });
    }
    walked
}

/// Assert every painted control did something, with a message that names the
/// dead one rather than just failing a count.
fn assert_every_painted_control_had_effect(walked: &[Walked]) {
    let painted: Vec<&Walked> = walked.iter().filter(|w| w.painted).collect();
    assert!(
        !painted.is_empty(),
        "no control painted — nothing was actually exercised"
    );
    let dead: Vec<&str> = painted
        .iter()
        .filter(|w| !w.had_effect())
        .map(|w| w.id.as_str())
        .collect();
    assert!(
        dead.is_empty(),
        "clicked these controls and NOTHING observable happened — no backend \
         call, no state change. A dead handler, a control with no listener, or \
         a handler wired to something that no longer runs: {dead:?}"
    );
}

// ── Fixtures ─────────────────────────────────────────────────────────

/// The Tools panel's state fingerprint: the fields its panel-walk already
/// treats as the panel's observable surface, plus the toggle-pending set that
/// a row click flips within the same frame.
fn tools_fingerprint(p: &ToolsPanel) -> String {
    format!(
        "loading={} error={:?} exts={} panels={} pending={:?}",
        p.loading,
        p.error,
        p.extensions.len(),
        p.panels.len(),
        p.pending_toggle,
    )
}

fn healthy_backend() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "ext.list",
            json!({ "extensions": [
                { "name": "ext-a", "version": "1.0.0", "enabled": true, "status": "running" },
            ]}),
        )
        .on("extensions.list_panels", json!({ "panels": [] }))
        .on(
            "ext.disable",
            json!({ "name": "ext-a", "version": "1.0.0", "enabled": false, "status": "stopped" }),
        )
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ToolsPanel> {
    // `add_window`, for the reason spelled out at the top of this file.
    let window = cx.add_window(|_w, cx| {
        let panel = ToolsPanel::new();
        // `spawn_refresh`, not `spawn_refresh_loop` — the walk must own every
        // backend call it counts. A background poll landing mid-walk would
        // move the counter on its own and let a dead button read as alive.
        ToolsPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

// ── The walk ─────────────────────────────────────────────────────────

#[gpui::test]
fn every_tools_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy_backend();
    let _guard = fake.clone().install();

    let window = mount(cx);
    let walked = walk(cx, window, &fake, tools_fingerprint);

    assert_every_painted_control_had_effect(&walked);
}

/// The walk must actually reach the controls this panel has — otherwise a
/// future refactor that stops registering them turns the test above into a
/// vacuous pass.
#[gpui::test]
fn the_walk_covers_the_controls_tools_renders(cx: &mut TestAppContext) {
    let fake = healthy_backend();
    let _guard = fake.clone().install();

    let window = mount(cx);
    let walked = walk(cx, window, &fake, tools_fingerprint);

    let painted: Vec<&str> = walked
        .iter()
        .filter(|w| w.painted)
        .map(|w| w.id.as_str())
        .collect();
    assert!(
        painted.contains(&"tools-refresh"),
        "the Refresh button painted and was walked; got {painted:?}"
    );
    assert!(
        painted.contains(&"ext-toggle::ext-a"),
        "the per-extension Enable/Disable toggle painted and was walked; got {painted:?}"
    );
}

/// The click has to go through **hit-testing**, not just exist. This pins the
/// effect to the specific verb the Refresh button is supposed to fire, so a
/// walk that accidentally clicked some other element (or nothing) can't pass
/// on an unrelated state change.
#[gpui::test]
fn clicking_refresh_reaches_the_real_listener(cx: &mut TestAppContext) {
    let fake = healthy_backend();
    let _guard = fake.clone().install();

    let window = mount(cx);
    let before = fake.count_for("ext.list");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    registry::begin_frame();
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();

    let bounds = vcx
        .debug_bounds("tools-refresh")
        .expect("the Refresh button painted");
    vcx.simulate_click(bounds.center(), Modifiers::none());
    vcx.run_until_parked();

    assert_eq!(
        fake.count_for("ext.list"),
        before + 1,
        "a click at the Refresh button's painted centre fired exactly one \
         catalog re-read through the panel's own listener"
    );
}

/// The other half of hit-testing: a click at a point the control does **not**
/// occupy must not fire it. Without this, a walk that clicked the window
/// origin every time would pass on any panel that reloads on any click.
#[gpui::test]
fn a_click_away_from_a_control_fires_nothing(cx: &mut TestAppContext) {
    let fake = healthy_backend();
    let _guard = fake.clone().install();

    let window = mount(cx);

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    registry::begin_frame();
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();

    let refresh = vcx
        .debug_bounds("tools-refresh")
        .expect("the Refresh button painted");
    let before = fake.count_for("ext.list");

    // Low-left of the window — inside the panel's own background, far below
    // the header and any extension row.
    let empty_spot = gpui::point(
        px(8.0),
        vcx.update(|w, _| w.viewport_size().height) - px(8.0),
    );
    vcx.simulate_click(empty_spot, Modifiers::none());
    vcx.run_until_parked();

    assert_eq!(
        fake.count_for("ext.list"),
        before,
        "clicking empty panel background fired no verb — so the effects this \
         walk observes are attributable to the control it aimed at"
    );
    assert!(
        refresh.size.width > px(0.0) && refresh.size.height > px(0.0),
        "and the control the walk aims at has real, non-degenerate bounds"
    );
}

/// The registry is per-frame, not cumulative. A control that stopped painting
/// must stop being walked — otherwise the walk would click stale bounds and
/// report a phantom control as dead.
#[gpui::test]
fn a_control_that_stops_painting_leaves_the_walked_set(cx: &mut TestAppContext) {
    // No extensions → no per-extension toggle row, only the header Refresh.
    let fake = ScriptedBackend::new()
        .on("ext.list", json!({ "extensions": [] }))
        .on("extensions.list_panels", json!({ "panels": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);
    let walked = walk(cx, window, &fake, tools_fingerprint);

    let ids: Vec<&str> = walked.iter().map(|w| w.id.as_str()).collect();
    assert!(
        ids.contains(&"tools-refresh"),
        "the always-present control is still walked; got {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.starts_with("ext-toggle::")),
        "an empty catalog renders no toggle, so none is walked; got {ids:?}"
    );
    assert_every_painted_control_had_effect(&walked);
}

/// #247's third failure shape: a click that drives the panel into a branch
/// `panel_walk` never repaints. Here the catalog read fails, so the click
/// lands on the error-strip layout — a *different* element tree from the one
/// mount produced. A panic in that branch surfaces as a red test.
#[gpui::test]
fn controls_survive_being_clicked_in_the_error_branch(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("ext.list", "pipe_unavailable: extension-bridge not running")
        .on_err("extensions.list_panels", "pipe_unavailable: bridge down");
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "the fixture really is the error branch"
            );
        })
        .unwrap();

    let walked = walk(cx, window, &fake, tools_fingerprint);

    // Refresh still fires its verb while the panel is showing an error — the
    // user's one way out of a broken state must not itself be dead.
    let refresh = walked
        .iter()
        .find(|w| w.id == "tools-refresh")
        .expect("Refresh is walked in the error branch too");
    assert!(
        refresh.painted && refresh.had_effect(),
        "the recovery control still works when the panel is in its error state"
    );
}
