//! L7 **control**-walk — RemoteAccess (issue #247).
//!
//! Harness: `wylde_gui_test_support::control_walk`. Adding a control here
//! needs no edit to this file.
//!
//! RemoteAccess is the action-less panel: it issues path-routed `GET
//! /api/link/*` calls with no `"action"` envelope, so its fixture uses
//! `on_path` rather than `on`. The walk's backend channel counts those the
//! same way.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_remote_access::RemoteAccessPanel;

const STATUS: &str = "/api/link/status";
const CONFIG: &str = "/api/link/config";
const PEERS: &str = "/api/link/peers";

/// RemoteAccess surfaces only status failures, so `last_error` plus the row
/// counts and the load flags are its observable surface.
fn fingerprint(p: &RemoteAccessPanel) -> String {
    format!(
        "peers={} services={} err={:?} status_read={} loaded={}",
        p.peers.len(),
        p.services.len(),
        p.last_error,
        p.status_ever_read,
        p.initial_load_done,
    )
}

/// A peer row, so the per-peer control paints rather than only the header.
fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on_path(
            STATUS,
            json!({
                "enabled": true,
                "interface_up": true,
                "listen_port": 51820,
                "public_key": "abcDEF123456=",
            }),
        )
        .on_path(CONFIG, json!({}))
        .on_path(
            PEERS,
            json!({ "peers": [
                { "public_key": "peerKEY1=", "name": "tablet", "allowed_ips": ["10.0.0.2/32"] },
            ]}),
        )
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<RemoteAccessPanel> {
    // Only a poll loop exists here; gpui's simulated test clock never advances
    // on its own, so the leading iteration runs and the loop then parks. The
    // walk still owns every backend call it counts.
    let window = cx.add_window(|_w, cx| {
        let panel = RemoteAccessPanel::new();
        RemoteAccessPanel::spawn_refresh_loop(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<RemoteAccessPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/remote_access_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_remote_access_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_walk_covers_the_controls_remote_access_renders(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    assert!(
        painted.contains(&"remote-refresh"),
        "the Refresh control painted and was walked; got {painted:?}"
    );
    assert!(
        painted.iter().any(|id| id.starts_with("remote-peer::")),
        "the per-peer row painted and was walked; got {painted:?}"
    );
}

/// The link being down is the panel's most likely real state, and the branch
/// `panel_walk` cannot repaint after a click.
#[gpui::test]
fn controls_survive_being_clicked_when_the_link_is_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_path_err(STATUS, "pipe_unavailable: wyldelink down")
        .on_path_err(CONFIG, "pipe_unavailable: wyldelink down")
        .on_path_err(PEERS, "pipe_unavailable: wyldelink down");
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake).assert_every_control_lives();
}
