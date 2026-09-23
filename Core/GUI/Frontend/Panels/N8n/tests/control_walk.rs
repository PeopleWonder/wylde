//! L7 **control**-walk — Workflows (n8n).
//!
//! Every control that painted is clicked through gpui hit-testing and the real
//! listener, and must produce an observable effect. The harness lives in
//! `wylde_gui_test_support::control_walk`; read its module docs for the oracle.
//!
//! **Adding a control to this panel needs no edit here.** Build it with
//! `controls::control(div(), "id")` and it is registered, painted, walked and
//! required to do something automatically.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_n8n::N8nPanel;

/// The panel's observable surface: engine state, editor URL, reload count.
fn fingerprint(p: &N8nPanel) -> String {
    format!("state={:?} url={} reloads={}", p.state, p.url, p.reloads)
}

fn running() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new().on(
        "n8n.health",
        json!({ "reachable": true, "url": "http://127.0.0.1:5678", "auth_configured": true }),
    )
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<N8nPanel> {
    // `add_window` (not `open_window`) and `spawn_refresh` (not the poll
    // loop) — both load-bearing; see the harness module docs.
    let window = cx.add_window(|_w, cx| {
        let panel = N8nPanel::new();
        N8nPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<N8nPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/n8n_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_workflows_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = running();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

/// The walk must actually reach the controls — otherwise the test above is a
/// vacuous pass the day the panel stops painting them.
#[gpui::test]
fn the_walk_covers_reload_and_open_in_browser(cx: &mut TestAppContext) {
    let fake = running();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    assert!(painted.contains(&"n8n-reload"), "got {painted:?}");
    assert!(painted.contains(&"n8n-open-browser"), "got {painted:?}");
}

/// Reload must reach the real listener: it re-checks the engine.
#[gpui::test]
fn clicking_reload_rechecks_the_engine(cx: &mut TestAppContext) {
    let fake = running();
    let _guard = fake.clone().install();
    let window = mount(cx);
    let before = fake.count_for("n8n.health");

    walk(cx, window, &fake);

    assert!(
        fake.count_for("n8n.health") > before,
        "clicking Reload at its painted centre fired a fresh n8n.health"
    );
}

/// With the engine down there is no editor to open, so only Reload paints —
/// and it must still work, since it is the user's one way out.
#[gpui::test]
fn reload_lives_when_the_engine_is_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "n8n.health",
        json!({ "reachable": false, "url": "http://127.0.0.1:5678", "auth_configured": true }),
    );
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let ids: Vec<&str> = report.walked.iter().map(|w| w.id.as_str()).collect();
    assert!(ids.contains(&"n8n-reload"), "got {ids:?}");
    assert!(
        !ids.contains(&"n8n-open-browser"),
        "no editor to open while the engine is down; got {ids:?}"
    );
    report.assert_every_control_lives();
}

/// The error branch (`wylde-n8n` itself not answering) paints a different
/// tree; clicking through it must not panic or go dead.
#[gpui::test]
fn controls_survive_the_service_being_down(cx: &mut TestAppContext) {
    let fake =
        ScriptedBackend::new().on_err("n8n.health", "pipe_unavailable: wylde-n8n not running");
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake).assert_every_control_lives();
}
