//! L7 **control**-walk — Tools (issue #247).
//!
//! `panel_walk.rs` next door proves this panel *loads*. This proves its
//! controls *work*: every control that actually painted is clicked through
//! gpui hit-testing and the real listener, and must produce an observable
//! effect.
//!
//! The harness lives in `wylde_gui_test_support::control_walk` — read its
//! module docs for the oracle, the modal-state mechanism, and the
//! `add_window`-not-`open_window` trap. This file is what a panel actually
//! writes: a fixture, a fingerprint, and a call.
//!
//! **Adding a control to this panel needs no edit here.** Build it with
//! `controls::control(div(), "id")` and it is registered, painted, walked and
//! required to do something automatically.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_tools::ToolsPanel;

/// The Tools panel's observable surface: the fields `panel_walk.rs` already
/// asserts on, plus the toggle-pending set a row click flips in-frame.
fn fingerprint(p: &ToolsPanel) -> String {
    format!(
        "loading={} error={:?} exts={} panels={} pending={:?}",
        p.loading,
        p.error,
        p.extensions.len(),
        p.panels.len(),
        p.pending_toggle,
    )
}

fn healthy() -> std::sync::Arc<ScriptedBackend> {
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
    // `add_window` (not `open_window`) and `spawn_refresh` (not the poll
    // loop) — both load-bearing; see the harness module docs.
    let window = cx.add_window(|_w, cx| {
        let panel = ToolsPanel::new();
        ToolsPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<ToolsPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/tools_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_tools_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

/// The walk must actually reach this panel's controls — otherwise the test
/// above becomes a vacuous pass the day the registry stops filling.
#[gpui::test]
fn the_walk_covers_the_controls_tools_renders(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    assert!(
        painted.contains(&"tools-refresh"),
        "the Refresh button painted and was walked; got {painted:?}"
    );
    assert!(
        painted.contains(&"ext-toggle::ext-a"),
        "the per-extension toggle painted and was walked; got {painted:?}"
    );
}

/// The click must go through **hit-testing**, not merely exist. Pinning the
/// effect to the specific verb Refresh fires stops a walk that accidentally
/// clicked something else (or nothing) from passing on an unrelated change.
#[gpui::test]
fn clicking_refresh_reaches_the_real_listener(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);
    let before = fake.count_for("ext.list");

    walk(cx, window, &fake);

    assert!(
        fake.count_for("ext.list") > before,
        "clicking Refresh at its painted centre fired a catalog re-read through \
         the panel's own listener"
    );
}

/// #247's third failure shape: a click that drives the panel into a branch
/// `panel_walk` never repaints. Here the catalog read fails, so the click
/// lands on the error-strip layout — a different element tree from the one
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

    // Refresh is the user's one way out of a broken state; it must not itself
    // be dead there.
    walk(cx, window, &fake).assert_every_control_lives();
}

/// The registry is per-frame, not cumulative. A control that stops painting
/// must stop being walked, or the walk would click stale bounds and report a
/// phantom control as dead.
#[gpui::test]
fn a_control_that_stops_painting_leaves_the_walked_set(cx: &mut TestAppContext) {
    // No extensions → no per-extension toggle row, only the header Refresh.
    let fake = ScriptedBackend::new()
        .on("ext.list", json!({ "extensions": [] }))
        .on("extensions.list_panels", json!({ "panels": [] }));
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let ids: Vec<&str> = report.walked.iter().map(|w| w.id.as_str()).collect();
    assert!(
        ids.contains(&"tools-refresh"),
        "the always-present control is still walked; got {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.starts_with("ext-toggle::")),
        "an empty catalog renders no toggle, so none is walked; got {ids:?}"
    );
    report.assert_every_control_lives();
}
