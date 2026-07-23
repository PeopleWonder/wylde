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
//! * **state** — the panel's own [`ControlWalk::fingerprint`] closure. One per
//!   panel, not one per control.
//!
//! A control passes if **either** moved. Deliberately weak per control and
//! strong in aggregate: it cannot tell you the button did the *right* thing,
//! but it cannot be satisfied by a button that does *nothing* — which is the
//! class #247 is about. Per-control behavioural depth stays in ordinary
//! windowed tests next to the walk.
//!
//! Nav and modal effects fold into the same two channels: nav publishes on a
//! bus the panel reads back, and a modal opening *is* panel state.
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

use gpui::{Modifiers, Render, TestAppContext, VisualTestContext, WindowHandle};

use crate::ScriptedBackend;
use wylde_gui_controls::scan::literal_control_ids;

// ── The walk ─────────────────────────────────────────────────────────

/// What a click is allowed to change, sampled either side of it.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Effect {
    backend_calls: usize,
    state: String,
}

/// One walked control and what its click did.
#[derive(Debug, Clone)]
pub struct Walked {
    /// The control's registered id.
    pub id: String,
    /// Which named state painted it (`"default"` unless a [`ControlWalk::state`] did).
    pub state: String,
    /// Whether it was laid out and therefore clickable.
    pub painted: bool,
    before: Effect,
    after: Effect,
}

impl Walked {
    /// The oracle: a control "did something" if either channel moved.
    pub fn had_effect(&self) -> bool {
        self.painted && self.before != self.after
    }
}

type StateFn<V> = Box<dyn Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>)>;

/// Builder for a control walk over one panel.
pub struct ControlWalk<'a, V: Render + 'static> {
    window: WindowHandle<V>,
    fake: &'a Arc<ScriptedBackend>,
    fingerprint: Option<Box<dyn Fn(&V) -> String>>,
    states: Vec<(String, StateFn<V>)>,
    sources: Vec<&'static str>,
}

impl<'a, V: Render + 'static> ControlWalk<'a, V> {
    /// Start a walk over an already-mounted panel.
    pub fn new(window: WindowHandle<V>, fake: &'a Arc<ScriptedBackend>) -> Self {
        Self {
            window,
            fake,
            fingerprint: None,
            states: Vec::new(),
            sources: Vec::new(),
        }
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

    /// Draw, enumerate, click.
    pub fn run(self, cx: &mut TestAppContext) -> WalkReport {
        let fingerprint = self.fingerprint.expect(
            "ControlWalk::fingerprint is required — without it the oracle has only one channel",
        );
        let mut vcx = VisualTestContext::from_window(self.window.into(), cx);
        let mut walked: Vec<Walked> = Vec::new();

        // The default frame, then each declared state.
        walk_one_state(
            &mut vcx,
            self.window,
            self.fake,
            &fingerprint,
            "default",
            &mut walked,
        );
        for (label, apply) in &self.states {
            self.window
                .update(&mut vcx, |panel, window, cx| apply(panel, window, cx))
                .expect("the panel entity is still alive");
            vcx.run_until_parked();
            walk_one_state(
                &mut vcx,
                self.window,
                self.fake,
                &fingerprint,
                label,
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
        }
    }
}

fn walk_one_state<V: Render + 'static>(
    vcx: &mut VisualTestContext,
    window: WindowHandle<V>,
    fake: &Arc<ScriptedBackend>,
    fingerprint: &dyn Fn(&V) -> String,
    state_label: &str,
    out: &mut Vec<Walked>,
) {
    // Fresh frame: gpui clears its own `debug_bounds` at the top of every
    // real frame, so after this the constructed-half and the painted-half
    // describe the same tree.
    wylde_gui_controls::registry::begin_frame();
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();

    for id in wylde_gui_controls::registry::constructed() {
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
        // Already clicked in an earlier state — a control present in both the
        // default frame and a modal frame is the same control.
        if out.iter().any(|w| w.id == id.as_ref() && w.painted) {
            continue;
        }

        let snap = |vcx: &mut VisualTestContext| Effect {
            backend_calls: fake.calls().len(),
            state: window
                .update(vcx, |panel, _w, _cx| fingerprint(panel))
                .expect("the panel entity is still alive"),
        };

        let before = snap(vcx);
        // A real platform mouse event at the control's painted centre, routed
        // through gpui hit-testing to whatever listener the panel attached.
        // A panicking listener kills the test here — the correct outcome.
        vcx.simulate_click(bounds.center(), Modifiers::none());
        vcx.run_until_parked();
        let after = snap(vcx);

        out.push(Walked {
            id: id.to_string(),
            state: state_label.to_string(),
            painted: true,
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

    /// **The core assertion.** Every control that painted did something.
    pub fn assert_every_control_lives(self) -> Self {
        let painted: Vec<&Walked> = self.walked.iter().filter(|w| w.painted).collect();
        assert!(
            !painted.is_empty(),
            "no control painted — nothing was exercised. Either this panel routes \
             no control through `controls::control()`, or the draw never happened. \
             An empty walk is a disarmed gate, not a pass."
        );
        let dead: Vec<String> = painted
            .iter()
            .filter(|w| !w.had_effect())
            .map(|w| format!("{} (state: {})", w.id, w.state))
            .collect();
        assert!(
            dead.is_empty(),
            "clicked these controls and NOTHING observable happened — no backend \
             call, no state change. A dead handler, a control with no listener, or \
             a handler wired to something that no longer runs: {dead:?}"
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
