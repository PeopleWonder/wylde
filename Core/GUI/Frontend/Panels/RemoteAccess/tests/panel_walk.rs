//! L7 panel-walk — RemoteAccess / WyldeLink (issue #35, roadmap T0.1b).
//!
//! One of the four zero-coverage panels, and the one with the unusual wire
//! shape: it talks to `wylde-vpn` with HTTP-style `GET /api/link/*` calls that
//! carry NO `"action"` envelope. The scripted fake therefore can't route those
//! by action — this suite drives them through the new path-based
//! `on_path` / `on_path_err` seam added to `ScriptedBackend` for exactly this
//! panel. Mount the real `RemoteAccessPanel` the way the Shell does (`new()` +
//! `spawn_refresh_loop`).
//!
//! **What "error state" means for RemoteAccess:** a `last_error: Option<String>`
//! set ONLY when the status read fails at the transport (config/peers/services
//! failures degrade silently per-card, Dashboard-style). The "no link yet"
//! signal is `status.is_unknown()`. The gate: `last_error.is_none()` +
//! `!status.is_unknown()` when wylde-vpn answers; `last_error.is_some()` when
//! it's down; `is_unknown()` + no error when it answers empty.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_remote_access::RemoteAccessPanel;

const STATUS: &str = "/api/link/status";

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<RemoteAccessPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = RemoteAccessPanel::new();
        RemoteAccessPanel::spawn_refresh_loop(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn remote_access_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_path(
            STATUS,
            json!({
                "enabled": true,
                "interface_up": true,
                "listen_port": 51820,
                "public_key": "abcDEF123456=",
            }),
        )
        .on_path(
            "/api/link/config",
            json!({ "public_host": "wylde.example.com" }),
        )
        .on_path("/api/link/peers", json!({ "peers": [] }))
        .on_path("/api/link/services", json!({ "services": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.last_error.is_none(),
                "no error when wylde-vpn answers"
            );
            assert!(panel.initial_load_done, "the first refresh cycle completed");
            assert!(panel.status_ever_read, "the status read landed");
            assert!(!panel.status.is_unknown(), "a live link is not 'unknown'");
        })
        .unwrap();
    assert_eq!(
        fake.count_for_path(STATUS),
        1,
        "the status route was read once"
    );
}

#[gpui::test]
fn remote_access_survives_backend_down(cx: &mut TestAppContext) {
    // wylde-vpn down: every route fails. Only the status failure surfaces on
    // `last_error` (the rest degrade per-card) — and the panel must not panic.
    let fake = ScriptedBackend::new()
        .on_path_err(STATUS, "pipe_unavailable: wylde-vpn not running")
        .on_path_err(
            "/api/link/config",
            "pipe_unavailable: wylde-vpn not running",
        )
        .on_path_err("/api/link/peers", "pipe_unavailable: wylde-vpn not running")
        .on_path_err(
            "/api/link/services",
            "pipe_unavailable: wylde-vpn not running",
        );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.last_error.is_some(),
                "a down wylde-vpn surfaces the status failure — not silent"
            );
            assert!(panel.initial_load_done, "the loader still completes");
        })
        .unwrap();
}

#[gpui::test]
fn remote_access_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_path_err(STATUS, "internal_error: vpn control blew up");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.last_error.is_some(),
                "an error envelope on status surfaces"
            );
            assert!(panel.initial_load_done);
        })
        .unwrap();
}

#[gpui::test]
fn remote_access_tolerates_empty_backend(cx: &mut TestAppContext) {
    // Default fake → Ok({}) for every path; the link reads 'unknown' (offline)
    // but that is NOT an error — it's the pre-configured state.
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.last_error.is_none(),
                "an empty envelope is not an error"
            );
            assert!(panel.initial_load_done);
            assert!(
                panel.status.is_unknown(),
                "an empty status envelope reads as 'unknown / not configured'"
            );
        })
        .unwrap();
}
