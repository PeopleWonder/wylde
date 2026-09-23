//! L7 panel-walk — Workflows (n8n).
//!
//! Mount the real `N8nPanel` the way the Shell does (`new()` + `spawn_refresh`),
//! drive `n8n.health` through the scripted fake, and assert the panel lands in
//! the right state for every realistic backend condition — never stuck on
//! "Checking".
//!
//! Backend conditions: engine running · engine down · service down.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_n8n::{EngineState, N8nPanel};

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<N8nPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = N8nPanel::new();
        N8nPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn a_running_engine_mounts_ready(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "n8n.health",
        json!({ "reachable": true, "url": "http://127.0.0.1:5999", "auth_configured": true }),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.state, EngineState::Ready);
            assert_eq!(panel.url, "http://127.0.0.1:5999", "the reported URL wins");
        })
        .unwrap();
    assert_eq!(fake.count_for("n8n.health"), 1);
}

#[gpui::test]
fn a_stopped_engine_says_so(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "n8n.health",
        json!({ "reachable": false, "url": "http://127.0.0.1:5678", "auth_configured": true }),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                matches!(panel.state, EngineState::Unreachable(_)),
                "got {:?}",
                panel.state
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_silent_service_is_surfaced_not_swallowed(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err("n8n.health", "pipe_unavailable: wylde-n8n down");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| match &panel.state {
            EngineState::Unreachable(why) => assert!(why.contains("wylde-n8n"), "{why}"),
            other => panic!("expected Unreachable, got {other:?}"),
        })
        .unwrap();
}
