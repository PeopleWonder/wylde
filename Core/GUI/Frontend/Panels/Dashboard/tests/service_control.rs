//! L7 Tier-C — Dashboard service console: the Stop control (issue #35).
//!
//! Closes #35's last named Tier-C control, "start/stop service". Start and
//! restart were already covered in Workspaces
//! (`Workspaces/tests/registry_nav.rs`), but **nothing in the GUI drove
//! `service.stop`** — the backend verb existed (`rust/.../control.rs`, parity
//! `lifecycle.rs:208`) yet no control did. This drives the new Dashboard
//! `spawn_stop_service` and asserts it dispatches the lifecycle `service.stop`
//! verb against the daemon with the chosen service name, then re-probes that
//! service — mirroring `start_service_affordance_issues_lifecycle_start`.
//!
//! It rides the required `gui panel-walk (L7)` check: the alias runs every test
//! target in the panel crates (issue #56 dropped the `--test panel_walk`
//! filter), so this file is gated, not a rot-prone extra.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_dashboard::DashboardPanel;

/// Mount the panel the way `DashboardPanel::view` does — the same helper the
/// panel-walk uses — so the auto-refresh loop's first cycle has run and the
/// health strip is populated before the control is driven.
fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<DashboardPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = DashboardPanel::new();
        DashboardPanel::spawn_refresh_loop(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn stop_control_issues_lifecycle_stop(cx: &mut TestAppContext) {
    // The console's Stop control drives `service.stop` against the lifecycle
    // daemon for the chosen service, then re-probes that one service's health
    // so its chip reflects the new state without waiting for the 5 s refresh.
    let fake = ScriptedBackend::new()
        .on(
            "service.stop",
            json!({ "name": "wylde-gateway", "status": "stopped" }),
        )
        .on("service.health", json!({ "ok": true }));
    let _guard = fake.clone().install();

    let window = mount(cx);
    // Baseline: every health probe the mount refresh already fired. The
    // re-probe assertion below must see a *new* one on top of these, or it
    // proves nothing (the strip probes all services on mount).
    let health_probes_after_mount = fake.count_for("service.health");

    window
        .update(cx, |panel, _w, cx| {
            panel.spawn_stop_service("wylde-gateway".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    let stop = fake
        .last_call_for("service.stop")
        .expect("the Stop control must dispatch the lifecycle service.stop verb");
    assert_eq!(
        stop.service, "wylde-lifecycle",
        "service control hits the lifecycle daemon, not the target service directly"
    );
    assert_eq!(
        stop.payload_str("name").as_deref(),
        Some("wylde-gateway"),
        "the console stops exactly the service the user chose"
    );
    // Re-probe happened: a stop refreshes that service's chip immediately
    // rather than leaving a stale green until the next auto-refresh tick.
    // Deleting the re-probe from `spawn_stop_service` fails this.
    assert!(
        fake.count_for("service.health") > health_probes_after_mount,
        "a stop re-probes the stopped service's health"
    );
}

#[gpui::test]
fn stop_control_does_not_touch_the_start_verb(cx: &mut TestAppContext) {
    // Guard against a copy-paste regression: stop must use `service.stop`, never
    // `service.start`/`service.restart` (the recovery verbs). A wrong verb here
    // would silently (re)start the service the user asked to stop.
    let fake = ScriptedBackend::new()
        .on(
            "service.stop",
            json!({ "name": "wylde-harness", "status": "stopped" }),
        )
        .on("service.health", json!({ "ok": true }));
    let _guard = fake.clone().install();

    let window = mount(cx);
    let starts_before = fake.count_for("service.start") + fake.count_for("service.restart");

    window
        .update(cx, |panel, _w, cx| {
            panel.spawn_stop_service("wylde-harness".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        fake.count_for("service.stop"),
        1,
        "the control fires exactly one stop"
    );
    assert_eq!(
        fake.count_for("service.start") + fake.count_for("service.restart"),
        starts_before,
        "stopping a service must never issue a start/restart"
    );
}
