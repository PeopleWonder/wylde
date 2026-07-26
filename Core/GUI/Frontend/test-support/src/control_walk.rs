//! The shared control-walk harness (issue #247).
//!
//! One place that knows how to click every control a panel paints and prove
//! something happened. A panel's `tests/control_walk.rs` should be a fixture,
//! a fingerprint, and a call — not a copy of this logic.
//!
//! # What a panel has to write
//!
//! ```ignore
//! use wylde_gui_test_support::{control_walk::ControlWalk, ScriptedBackend};
//!
//! #[gpui::test]
//! fn every_control_does_something(cx: &mut gpui::TestAppContext) {
//!     let fake = ScriptedBackend::new().on("ext.list", json!({ "extensions": [] }));
//!     let _guard = fake.clone().install();
//!     let window = cx.add_window(|_w, cx| { let p = ToolsPanel::new(); ToolsPanel::spawn_refresh(cx); p });
//!     cx.run_until_parked();
//!
//!     ControlWalk::new(window, &fake)
//!         .fingerprint(|p: &ToolsPanel| format!("{} {:?}", p.loading, p.error))
//!         .sources(&[include_str!("../src/tools_panel.rs")])
//!         .run(cx)
//!         .assert_every_control_lives()
//!         .assert_covers_every_literal_id();
//! }
//! ```
//!
//! **Adding a control after that is a one-liner.** Build it with
//! `controls::control(div(), "my-id")` and it is registered, painted, walked,
//! clicked, and required to do something — with no edit to any test. That is
//! the whole point of routing every control through one constructor: coverage
//! is a property of construction, not of somebody remembering to add a case.
//!
//! # The oracle
//!
//! "Something observable happened" is a **delta** over two cheap channels
//! sampled either side of the click:
//!
//! * **backend** — [`ScriptedBackend`] records every call; a click that fires
//!   IPC moves the count.
//! * **nav** — `wylde_gui_pipe::request_nav`, the cross-panel navigation
//!   channel, recorded by the pipe crate's dev-only `nav_probe`.
//! * **state** — the panel's own [`ControlWalk::fingerprint`] closure. One per
//!   panel, not one per control.
//!
//! A control passes if **any** of the three moved. Deliberately weak per control and
//! strong in aggregate: it cannot tell you the button did the *right* thing,
//! but it cannot be satisfied by a button that does *nothing* — which is the
//! class #247 is about. Per-control behavioural depth stays in ordinary
//! windowed tests next to the walk.
//!
//! The nav channel is not optional garnish. An earlier version of this harness
//! had only backend + state, on the assumption that "nav publishes on a bus the
//! panel reads back". That is false: `request_nav` hands the key to the SHELL,
//! and the originating panel's own state never moves. Under a two-channel
//! oracle the Dashboard's fifteen service chips and its empty-state rows — all
//! of which do nothing but navigate — read as **dead controls**. They are not;
//! they are the reason this channel exists.
//!
//! Modal effects do fold into `state`: a modal opening *is* panel state.
//!
//! # Modal-gated controls
//!
//! A control that only paints once a modal is open is not in the default
//! frame's registry. Walking only the default frame would report it as
//! "covered" by never mentioning it — worse than not walking the panel at all,
//! because the number looks complete.
//!
//! [`ControlWalk::state`] adds a named state: a closure that drives the panel
//! into some condition (open a dialog, expand a section, select a row), after
//! which the walk redraws and walks whatever *that* frame paints. Every state
//! is walked, and coverage is asserted over the **union**.
//!
//! ```ignore
//! ControlWalk::new(window, &fake)
//!     .fingerprint(fp)
//!     .state("delete-confirm-open", |p: &mut MemoryPanel, _w, cx| {
//!         p.confirm_delete = Some("note-1".into());
//!         cx.notify();
//!     })
//!     .sources(&[include_str!("../src/memory_panel.rs")])
//!     .run(cx)
//! ```
//!
//! [`WalkReport::assert_covers_every_literal_id`] is what makes that
//! self-policing: it scans the declared sources for `control(..., "literal")`
//! ids and fails on any that no state ever painted. Add a modal control and
//! forget the state, and the walk tells you the id it never reached.
//!
//! # Mount with `add_window`, not `open_window`
//!
//! At gpui rev `b3d93d44`, `TestAppContext::open_window(size, …)` sets the
//! window's reported `viewport_size` but the root element still lays out
//! against the test *display*. Every control then paints outside the window,
//! `simulate_click` hits nothing, and **every control reads as dead** — a
//! total false positive shaped exactly like the bug this harness is for.
//! `add_window` maximizes to the test display, so layout and viewport agree.
//!
//! # Mount with the one-shot loader, not a poll loop
//!
//! A panel with a `spawn_refresh_loop` should be mounted with its one-shot
//! `spawn_refresh`. The walk must own every backend call it counts; a
//! background poll landing mid-walk moves the counter on its own and lets a
//! dead button read as alive.

use std::sync::Arc;

use gpui::{px, size, Modifiers, Render, Size, TestAppContext, VisualTestContext, WindowHandle};

use crate::ScriptedBackend;
use wylde_gui_controls::scan::literal_control_ids;

// ── The walk ─────────────────────────────────────────────────────────

/// What a click is allowed to change, sampled either side of it.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Effect {
    backend_calls: usize,
    /// Cross-panel nav requests made so far (`wylde_gui_pipe::request_nav`).
    nav_requests: usize,
    /// Cross-panel focus deep-links made so far
    /// (`wylde_gui_pipe::request_workspace_focus`) — the effect of the "view in
    /// graph" controls, which is neither a backend call, a nav, nor a change to
    /// the clicking panel's own state.
    focus_requests: usize,
    state: String,
}

/// One walked control and what its click did.
#[derive(Debug, Clone)]
pub struct Walked {
    /// The control's registered id.
    pub id: String,
    /// Which named state painted it (`"default"` unless a [`ControlWalk::state`] did).
    pub state: String,
    /// Whether it was laid out this frame (has painted bounds).
    pub painted: bool,
    /// Whether the click point actually lies inside the walk viewport.
    ///
    /// A control can paint with valid bounds and still be **unreachable** by a
    /// synthetic click — most simply, when it lays out below the viewport, so
    /// the click at its centre lands outside the window and hits nothing. That
    /// is not a dead handler; it is the walk failing to reach a live control,
    /// and it must be reported as its own thing, or a live-but-off-screen
    /// button gets slandered as dead. `false` only when `painted` is `true`.
    pub reachable: bool,
    before: Effect,
    after: Effect,
}

impl Walked {
    /// The oracle: a control "did something" if any channel moved. Only
    /// meaningful for a control that both painted and was reachable — an
    /// unreachable control was never actually clicked.
    pub fn had_effect(&self) -> bool {
        self.painted && self.reachable && self.before != self.after
    }
}

type StateFn<V> = Box<dyn Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>)>;

/// Builder for a control walk over one panel.
/// Default walk viewport — deliberately far taller than the test display so a
/// long page lays out in full and every control is inside the window.
pub const WALK_VIEWPORT: (f32, f32) = (1600.0, 6000.0);

pub struct ControlWalk<'a, V: Render + 'static> {
    window: WindowHandle<V>,
    viewport: Size<gpui::Pixels>,
    fake: &'a Arc<ScriptedBackend>,
    fingerprint: Option<Box<dyn Fn(&V) -> String>>,
    reset: Option<StateFn<V>>,
    states: Vec<(String, StateFn<V>)>,
    sources: Vec<&'static str>,
    external_effect: Vec<&'static str>,
}

impl<'a, V: Render + 'static> ControlWalk<'a, V> {
    /// Start a walk over an already-mounted panel.
    pub fn new(window: WindowHandle<V>, fake: &'a Arc<ScriptedBackend>) -> Self {
        Self {
            window,
            viewport: size(px(WALK_VIEWPORT.0), px(WALK_VIEWPORT.1)),
            fake,
            fingerprint: None,
            reset: None,
            states: Vec::new(),
            sources: Vec::new(),
            external_effect: Vec::new(),
        }
    }

    /// Declare controls whose effect happens **outside** anything the harness
    /// can observe — an OS-native dialog (a folder/file picker), a handoff to
    /// another process — so the walk still *clicks* them (a panic on click is
    /// still caught) but does not require an observable backend/nav/state
    /// delta afterward.
    ///
    /// This is NOT the escape hatch of last resort — it is narrow and honest.
    /// The `control-ok` `wylde_check` marker is for ids that are not clickable
    /// controls at all (scroll handles); this is for genuine controls whose
    /// only effect is un-observable *in a headless test*. Each id listed here
    /// must be justified at the call site: the control opens a native dialog
    /// (`rfd`), not "the fixture is hard to set up". A dead handler must never
    /// be hidden behind this — prefer widening the fingerprint or adding a
    /// state first, and reach for this only when the effect is truly external.
    pub fn external_effect(mut self, ids: &[&'static str]) -> Self {
        self.external_effect.extend_from_slice(ids);
        self
    }

    /// The panel's observable-state snapshot — one closure per panel.
    ///
    /// Fold in the fields the panel's own `panel_walk.rs` already treats as
    /// its surface, plus anything a click flips within the same frame
    /// (a pending set, an expanded flag, a selected id).
    pub fn fingerprint(mut self, f: impl Fn(&V) -> String + 'static) -> Self {
        self.fingerprint = Some(Box::new(f));
        self
    }

    /// Return the panel to a known baseline **before every individual click**.
    ///
    /// Needed whenever a control can open something that occludes the rest of
    /// the panel. Wylde's modals are `.absolute().inset_0().occlude()`
    /// backdrops: once one click opens one, every LATER click in that pass
    /// lands on the backdrop instead of its target, and a whole tail of
    /// perfectly live controls reports as dead. Clicks have to be independent,
    /// and only the panel knows how to close its own modals.
    ///
    /// ```ignore
    /// .reset(|p: &mut SettingsPanel, _w, cx| {
    ///     p.hf_modal_open = false;
    ///     p.auto_check_modal_open = false;
    ///     cx.notify();
    /// })
    /// ```
    pub fn reset(
        mut self,
        f: impl Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>) + 'static,
    ) -> Self {
        self.reset = Some(Box::new(f));
        self
    }

    /// Walk an additional named state — for controls that only paint once the
    /// panel is in some condition (a modal open, a section expanded).
    ///
    /// States are applied to a fresh frame each time, in declaration order,
    /// after the default frame has been walked.
    pub fn state(
        mut self,
        label: &str,
        f: impl Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>) + 'static,
    ) -> Self {
        self.states.push((label.to_string(), Box::new(f)));
        self
    }

    /// The panel sources whose literal control ids must all be walked.
    ///
    /// Pass them with `include_str!`, e.g.
    /// `.sources(&[include_str!("../src/memory_panel.rs")])`. `wylde_check`
    /// rule 59 checks that every source file in the crate which builds a
    /// control is declared here, so this list cannot silently fall behind.
    pub fn sources(mut self, sources: &[&'static str]) -> Self {
        self.sources.extend_from_slice(sources);
        self
    }

    /// Override the viewport the walk lays the panel out in.
    ///
    /// Defaults to [`WALK_VIEWPORT`]. Raise it if a panel is taller still;
    /// there is no cost beyond layout, since the test platform has no real
    /// display.
    pub fn viewport(mut self, size: Size<gpui::Pixels>) -> Self {
        self.viewport = size;
        self
    }

    /// Draw, enumerate, click.
    pub fn run(self, cx: &mut TestAppContext) -> WalkReport {
        let fingerprint = self.fingerprint.expect(
            "ControlWalk::fingerprint is required — without it the oracle has only one channel",
        );
        let mut vcx = VisualTestContext::from_window(self.window.into(), cx);

        // Grow the viewport before drawing. `add_window` sizes to the test
        // display (1920x1080), and a long page — Settings is ~1300px of
        // content — lays its lower controls out BELOW that. They still get
        // painted bounds, so they look walkable, but `simulate_click` at
        // y > 1080 lands outside the window and hits nothing: every control
        // past the fold reads as dead. That is the same false-positive shape
        // as the `open_window` trap, and it is why this is done here once
        // rather than left for each panel to trip over.
        //
        // A real user reaches those controls by scrolling; the walk reaches
        // them by making the window tall enough that there is nothing to
        // scroll. Deterministic, and costs only layout on a headless platform.
        vcx.simulate_resize(self.viewport);
        vcx.run_until_parked();

        let mut walked: Vec<Walked> = Vec::new();

        // The default frame, then each declared state.
        walk_one_state(
            &mut vcx,
            self.window,
            self.fake,
            &fingerprint,
            "default",
            self.reset.as_deref(),
            None,
            &mut walked,
        );
        for (label, apply) in &self.states {
            walk_one_state(
                &mut vcx,
                self.window,
                self.fake,
                &fingerprint,
                label,
                self.reset.as_deref(),
                Some(&**apply),
                &mut walked,
            );
        }

        let mut literal_ids: Vec<String> = Vec::new();
        for src in &self.sources {
            for id in literal_control_ids(src) {
                if !literal_ids.contains(&id) {
                    literal_ids.push(id);
                }
            }
        }
        WalkReport {
            walked,
            literal_ids,
            declared_sources: self.sources.len(),
            external_effect: self.external_effect,
        }
    }
}

fn walk_one_state<V: Render + 'static>(
    vcx: &mut VisualTestContext,
    window: WindowHandle<V>,
    fake: &Arc<ScriptedBackend>,
    fingerprint: &dyn Fn(&V) -> String,
    state_label: &str,
    reset: Option<&dyn Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>)>,
    enter: Option<&dyn Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>)>,
    out: &mut Vec<Walked>,
) {
    // Put the panel in this state's baseline, then find out what it paints.
    let rebase = |vcx: &mut VisualTestContext| {
        window
            .update(vcx, |panel, w, cx| {
                if let Some(r) = reset {
                    r(panel, w, cx);
                }
                if let Some(e) = enter {
                    e(panel, w, cx);
                }
            })
            .expect("the panel entity is still alive");
        vcx.run_until_parked();
    };
    rebase(vcx);
    // Fresh frame: gpui clears its own `debug_bounds` at the top of every
    // real frame, so after this the constructed-half and the painted-half
    // describe the same tree.
    wylde_gui_controls::registry::begin_frame();
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();

    let ids = wylde_gui_controls::registry::constructed();
    for id in ids {
        // Re-establish the baseline before EVERY click, not just once per
        // state. A click that opened an occluding modal would otherwise sit
        // over the panel for the rest of the pass and swallow every later
        // click — see `ControlWalk::reset`.
        rebase(vcx);
        let key_probe: &'static str = Box::leak(id.to_string().into_boxed_str());
        if vcx.debug_bounds(key_probe).is_none() {
            // The rebase stopped this control painting (it belonged to a state
            // an earlier click left). Nothing to click.
            continue;
        }
        // `debug_bounds` is keyed by `&'static str`; control ids are a small
        // bounded set per test binary, so leaking each once is cheaper than
        // threading a lifetime through the registry.
        let key: &'static str = Box::leak(id.to_string().into_boxed_str());
        let Some(bounds) = vcx.debug_bounds(key) else {
            // Constructed but never laid out — a collapsed section, or an
            // off-screen row. Recorded, not clicked: there is nowhere on
            // screen for the user to click either.
            out.push(Walked {
                id: id.to_string(),
                state: state_label.to_string(),
                painted: false,
                reachable: false,
                before: Effect {
                    backend_calls: 0,
                    nav_requests: 0,
                    focus_requests: 0,
                    state: String::new(),
                },
                after: Effect {
                    backend_calls: 0,
                    nav_requests: 0,
                    focus_requests: 0,
                    state: String::new(),
                },
            });
            continue;
        };
        // Already clicked in an earlier state — a control present in both the
        // default frame and a modal frame is the same control.
        if out.iter().any(|w| w.id == id.as_ref() && w.painted) {
            continue;
        }

        let snap = |vcx: &mut VisualTestContext| Effect {
            backend_calls: fake.calls().len(),
            nav_requests: wylde_gui_pipe::nav_bus::nav_probe::count(),
            focus_requests: wylde_gui_pipe::focus_bus::focus_probe::count(),
            state: window
                .update(vcx, |panel, _w, _cx| fingerprint(panel))
                .expect("the panel entity is still alive"),
        };

        // Reachability: a click only dispatches if the point is inside the
        // window. A control laid out below the viewport paints valid bounds
        // but cannot be clicked — record that as unreachable, distinct from a
        // dead handler, rather than clicking into the void and calling it dead.
        let viewport = vcx.update(|w, _| w.viewport_size());
        let center = bounds.center();
        let reachable = center.x >= gpui::px(0.0)
            && center.y >= gpui::px(0.0)
            && center.x <= viewport.width
            && center.y <= viewport.height;

        if !reachable {
            out.push(Walked {
                id: id.to_string(),
                state: state_label.to_string(),
                painted: true,
                reachable: false,
                before: Effect {
                    backend_calls: 0,
                    nav_requests: 0,
                    focus_requests: 0,
                    state: String::new(),
                },
                after: Effect {
                    backend_calls: 0,
                    nav_requests: 0,
                    focus_requests: 0,
                    state: String::new(),
                },
            });
            continue;
        }

        let before = snap(vcx);
        // A real platform mouse event at the control's painted centre, routed
        // through gpui hit-testing to whatever listener the panel attached.
        // A panicking listener kills the test here — the correct outcome.
        vcx.simulate_click(center, Modifiers::none());
        vcx.run_until_parked();
        let after = snap(vcx);

        out.push(Walked {
            id: id.to_string(),
            state: state_label.to_string(),
            painted: true,
            reachable: true,
            before,
            after,
        });
    }
}

// ── The report ───────────────────────────────────────────────────────

/// The outcome of a walk. The `assert_*` methods chain.
pub struct WalkReport {
    /// Every control seen, in walk order.
    pub walked: Vec<Walked>,
    /// Literal control ids found in the declared sources.
    pub literal_ids: Vec<String>,
    declared_sources: usize,
    external_effect: Vec<&'static str>,
}

impl WalkReport {
    /// Ids of controls that actually painted and were clicked.
    pub fn painted_ids(&self) -> Vec<&str> {
        self.walked
            .iter()
            .filter(|w| w.painted)
            .map(|w| w.id.as_str())
            .collect()
    }

    /// **The core assertion.** Every control that painted was reachable and
    /// did something.
    ///
    /// The two failure modes are reported separately on purpose. A control the
    /// walk could not *reach* (painted below the viewport) is a walk problem —
    /// grow `WALK_VIEWPORT` or scroll it in — not a dead control, and saying
    /// "dead handler" there sends you hunting a bug that isn't in the panel.
    /// A control that was clicked and produced no observable effect is the real
    /// #247 finding.
    pub fn assert_every_control_lives(self) -> Self {
        let painted: Vec<&Walked> = self.walked.iter().filter(|w| w.painted).collect();
        assert!(
            !painted.is_empty(),
            "no control painted — nothing was exercised. Either this panel routes \
             no control through `controls::control()`, or the draw never happened. \
             An empty walk is a disarmed gate, not a pass."
        );

        // Unreachable first: a control that was never actually clicked cannot be
        // judged dead, so report it as its own problem and stop.
        let unreachable: Vec<String> = painted
            .iter()
            .filter(|w| !w.reachable)
            .map(|w| format!("{} (state: {})", w.id, w.state))
            .collect();
        assert!(
            unreachable.is_empty(),
            "these controls painted but their click point fell OUTSIDE the walk \
             viewport, so the walk never actually clicked them — they are \
             unreachable, NOT dead: {unreachable:?}\n\
             The panel is taller than WALK_VIEWPORT ({}x{}). Raise it with \
             `.viewport(size(px(w), px(h)))`, or scroll the control into view. \
             Do not read this as a dead handler.",
            WALK_VIEWPORT.0,
            WALK_VIEWPORT.1,
        );

        // An `external_effect` control was clicked (so a panic on click is still
        // caught), but its effect is un-observable in a headless test — an
        // OS-native dialog, a process handoff — so no delta is required. Every
        // id declared must actually have painted, or the declaration is stale
        // and could be hiding a control that has since become genuinely dead.
        let stale_external: Vec<&str> = self
            .external_effect
            .iter()
            .filter(|id| !painted.iter().any(|w| w.id == **id))
            .copied()
            .collect();
        assert!(
            stale_external.is_empty(),
            "these ids are declared `external_effect` but never painted, so the \
             declaration is stale — remove them (or fix the state that should \
             paint them, so a now-dead control isn't hidden behind the exemption): \
             {stale_external:?}"
        );

        let dead: Vec<String> = painted
            .iter()
            .filter(|w| w.reachable && !w.had_effect())
            .filter(|w| !self.external_effect.contains(&w.id.as_str()))
            .map(|w| format!("{} (state: {})", w.id, w.state))
            .collect();
        assert!(
            dead.is_empty(),
            "clicked these controls and NOTHING observable happened — no backend \
             call, no nav, no state change. A dead handler, a control with no \
             listener, or a handler wired to something that no longer runs — OR a \
             fixture that doesn't set the precondition under which the effect is \
             visible (check the fingerprint covers the field it moves). If a \
             control genuinely triggers only an OS-native dialog, declare it via \
             `.external_effect(&[..])` with a reason: {dead:?}"
        );
        self
    }

    /// **The anti-false-coverage assertion.** Every literal control id in the
    /// declared sources was painted by some state.
    ///
    /// This is what stops a modal-gated control from being silently uncovered.
    /// Without it, a control the walk never reaches simply goes unmentioned,
    /// and the walk reports success over a smaller set than the panel has —
    /// coverage that looks complete because the missing part is invisible.
    ///
    /// Ids built at runtime (`format!("row::{}", id)`) carry no literal and are
    /// not checked here; they are covered by the rows the fixture renders, and
    /// `assert_every_control_lives` still clicks them.
    pub fn assert_covers_every_literal_id(self) -> Self {
        assert!(
            self.declared_sources > 0,
            "no sources declared — call `.sources(&[include_str!(\"../src/…\")])` so \
             the walk can tell which controls it is supposed to reach. Without it \
             this assertion passes vacuously."
        );
        let painted = self.painted_ids();
        let missed: Vec<&String> = self
            .literal_ids
            .iter()
            .filter(|id| !painted.contains(&id.as_str()))
            .collect();
        assert!(
            missed.is_empty(),
            "these controls exist in the panel source but NO walked state ever \
             painted them, so nothing clicked them: {missed:?}\n\
             If they are modal-gated, add a `.state(\"…\", |p, _w, cx| …)` that opens \
             the modal. Leaving them out is worse than not walking the panel: the \
             walk would report success over a smaller set than the panel has."
        );
        self
    }
}
