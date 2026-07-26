//! L7 **control**-walk — the Shell's nav chrome (issue #247).
//!
//! `wylde-gui-shell-chrome` holds the renderers the `wry`/tray-linking Shell
//! could not let the headless panel-walk build: the sidebar, the panel slot
//! (with its service-recovery affordance), and the bottom-left update pill +
//! changelog modal. They render generic over [`NavChromeHost`] instead of the
//! concrete `Shell`, so this walk supplies a fake host — [`ChromeHarness`] —
//! that *is* the host: every handler records an observable delta into a
//! fingerprinted field, which is exactly what proves each control is wired to a
//! host method (the method's real behaviour — IPC, install — stays the Shell's
//! concern, tested there).
//!
//! `ChromeHarness::render` mirrors the real `impl Render for Shell`
//! (`Shell/src/shell_root.rs`): sidebar + slot always, the update pill when
//! `show_pill`, the changelog modal when `show_changelog`.
//!
//! The pill's "Update" and "Ignore" buttons route through `control()` (they are
//! real affordances — Update kicks the whole-stack install, Ignore dismisses
//! this version), so the walk discovers and clicks them in the `pill` state and
//! asserts their host-method deltas (`updated` / `dismissed_version`) moved.
//! Their ids are bound params inside `pill_button`, not literals at the call,
//! so the static id-scan does not separately demand them — the `pill` state
//! (forced by the literal `wylde-update-pill-changelog`) paints all three.

use std::sync::Arc;

use gpui::{div, prelude::*, AnyView, Context, Render, TestAppContext, Window};

use wylde_gui_shell_chrome::{
    render_changelog_modal, render_sidebar, render_slot, render_update_pill, NavChromeHost,
    NavOrigin, NavRow, SlotState,
};
use wylde_gui_test_support::control_walk::ControlWalk;
use wylde_gui_test_support::ScriptedBackend;

// ── A trivial view to stand in for the changelog viewer ──────────────────
//
// The real modal centres a substantial `ChangelogView` card over a
// full-screen scrim. The scrim's only control effect is a backdrop click that
// closes the modal — but the walk clicks a control at its painted *centre*, and
// the scrim's centre is the screen centre, exactly where the card sits. A
// card with any area there would swallow that click (the card stops
// propagation), and the live scrim would read dead. An EMPTY view keeps the
// card zero-sized, so the scrim's centre stays clear and its close-on-backdrop
// handler is the one that fires. (The card itself is declared `external_effect`
// — its only handler is a deliberate `stop_propagation` no-op.)
struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

// ── The fake host / test view ────────────────────────────────────────────

struct ChromeHarness {
    // Renderer inputs.
    rows: Vec<NavRow>,
    selected_key: Option<String>,
    slot_state: SlotState,
    update_available: bool,
    show_pill: bool,
    show_changelog: bool,
    changelog_view: AnyView,

    // Observable deltas the walk's fingerprint reads.
    //
    // `nav_clicks` is a monotonic counter bumped by `on_nav_click`: clicking the
    // *active* nav row would set `selected_key` to the value it already holds (a
    // legit no-op in the real `NavModel::select`), so without the counter the
    // active row would read dead. The counter makes every nav-row click show a
    // delta regardless of which row is selected.
    nav_clicks: usize,
    last_started_service: Option<String>,
    updated: bool,
    dismissed_version: Option<String>,
}

fn nav_row(key: &str, title: &str) -> NavRow {
    NavRow {
        key: key.to_string(),
        origin: NavOrigin::FirstParty,
        title: title.to_string(),
        icon: None,
        order: 0,
        required_services: vec![],
    }
}

impl ChromeHarness {
    fn new(cx: &mut Context<Self>) -> Self {
        let changelog_view: AnyView = cx.new(|_cx| EmptyView).into();
        Self {
            // Two rows, one selected — so the sidebar has a non-active row to
            // click and slot_state has a real key.
            rows: vec![nav_row("core/chat", "Chat"), nav_row("core/settings", "Settings")],
            selected_key: Some("core/chat".to_string()),
            slot_state: SlotState::Mount {
                key: "core/chat".to_string(),
            },
            update_available: false,
            show_pill: false,
            show_changelog: false,
            changelog_view,
            nav_clicks: 0,
            last_started_service: None,
            updated: false,
            dismissed_version: None,
        }
    }
}

impl Render for ChromeHarness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Mirror `impl Render for Shell`: sidebar + slot in a relative flex row,
        // the pill and changelog modal overlaid on top when their flags are set.
        let mut root = div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .child(render_sidebar(
                &self.rows,
                self.selected_key.as_deref(),
                None, // ResourceSnapshot — the meter paints no controls
                self.update_available,
                window,
                cx,
            ))
            .child(render_slot(
                &self.slot_state,
                &self.rows,
                None, // mounted AnyView
                None, // IframeFrame
                window,
                cx,
            ));

        if self.show_pill {
            root = root.child(render_update_pill("0.3.0", cx));
        }

        if self.show_changelog {
            root = root.child(render_changelog_modal(&self.changelog_view, cx));
        }

        root
    }
}

impl NavChromeHost for ChromeHarness {
    fn on_nav_click(&mut self, key: &str) -> bool {
        self.selected_key = Some(key.to_owned());
        self.nav_clicks += 1;
        true
    }

    fn on_start_service_click(&mut self, service: Arc<str>, cx: &mut Context<Self>) {
        self.last_started_service = Some(service.to_string());
        cx.notify();
    }

    fn open_changelog(&mut self, cx: &mut Context<Self>) {
        self.show_changelog = true;
        cx.notify();
    }

    fn on_update_click(&mut self, cx: &mut Context<Self>) {
        self.updated = true;
        cx.notify();
    }

    fn on_ignore_click(&mut self, version: String, cx: &mut Context<Self>) {
        self.dismissed_version = Some(version);
        cx.notify();
    }

    fn close_changelog(&mut self, cx: &mut Context<Self>) {
        self.show_changelog = false;
        cx.notify();
    }
}

// ── The walk ─────────────────────────────────────────────────────────────

#[gpui::test]
fn every_chrome_control_does_something(cx: &mut TestAppContext) {
    // The nav chrome makes no pipe calls (it dispatches to the host trait), so —
    // like the Changelog crate — installing the fake just keeps the oracle's
    // backend channel wired; it never moves. The state channel does the work.
    let fake = ScriptedBackend::new();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChromeHarness::new(cx));
    cx.run_until_parked();

    ControlWalk::new(window, &fake)
        .fingerprint(|h: &ChromeHarness| {
            format!(
                "sel={:?} nav={} started={:?} pill={} changelog={} updated={} dismissed={:?}",
                h.selected_key,
                h.nav_clicks,
                h.last_started_service,
                h.show_pill,
                h.show_changelog,
                h.updated,
                h.dismissed_version,
            )
        })
        // Baseline before every click: pill hidden, changelog closed, a known
        // selected row, the slot mounted (so no stray svc-start button).
        .reset(|h: &mut ChromeHarness, _w, cx| {
            h.selected_key = Some("core/chat".to_string());
            h.slot_state = SlotState::Mount {
                key: "core/chat".to_string(),
            };
            h.show_pill = false;
            h.show_changelog = false;
            h.updated = false;
            h.dismissed_version = None;
            h.last_started_service = None;
            // `nav_clicks` is intentionally NOT reset — it is a monotonic
            // counter, so it moves on every nav-row click regardless.
            cx.notify();
        })
        // Slot recovery affordance: an unavailable required service with a
        // `None` reason paints the `svc-start::{key}::{idx}` button (a `Some`
        // reason would suppress it — see `render_unavailable` in slot.rs).
        .state("svc-unavailable", |h: &mut ChromeHarness, _w, cx| {
            h.slot_state = SlotState::ServiceUnavailable {
                key: "core/chat".to_string(),
                missing: vec!["wylde-harness".to_string()],
                reasons: vec![None],
            };
            cx.notify();
        })
        // The update pill — paints its `control()`-routed "What's new"
        // affordance plus the Update and Ignore action buttons.
        .state("pill", |h: &mut ChromeHarness, _w, cx| {
            h.show_pill = true;
            cx.notify();
        })
        // The changelog modal — paints the scrim, card, and close button.
        .state("changelog-open", |h: &mut ChromeHarness, _w, cx| {
            h.show_changelog = true;
            cx.notify();
        })
        // The changelog card's only handler is a deliberate `stop_propagation`
        // no-op (it swallows clicks so they don't reach the backdrop). It is a
        // real, painted control with no observable delta — the exact case
        // `external_effect` is for.
        .external_effect(&["wylde-changelog-card"])
        .sources(&[
            include_str!("../src/sidebar.rs"),
            include_str!("../src/slot.rs"),
            include_str!("../src/update_pill.rs"),
        ])
        .run(cx)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}
