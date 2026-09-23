//! L7 panel-walk — Dashboard (issue #35, roadmap T0.1b).
//!
//! The concrete answer to "does EVERY page load, with error detection on each
//! page?" for the Dashboard — one of the four panels that had ZERO coverage
//! before this suite. Mount the real `DashboardPanel` the way the Shell does
//! (`new()` + `spawn_refresh_loop`), drive the mount-time IPC through the
//! scripted fake at the `wylde_gui_pipe::call` seam, and assert the panel
//! survives every realistic backend condition without panicking.
//!
//! **What "error state" means for the Dashboard:** unlike Models/Tools it has
//! NO panel-level `error` field — by design it *degrades per card* (a down
//! service turns one health dot red; the panel never becomes a wall of
//! banners). So the load-completed signal is `initial_load_done == true`, and
//! error DETECTION is per-service: a down daemon projects to
//! `HealthStatus::Unhealthy`, a healthy one to `HealthStatus::Healthy`. The
//! gate here is: the loader always runs to completion, and the health strip
//! reflects reality rather than the panel dying.
//!
//! Backend conditions covered: healthy · down/unavailable · error envelope ·
//! empty. See `docs/gui-testing.md`.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_dashboard::ipc::HealthStatus;
use wylde_panel_dashboard::DashboardPanel;

/// Mount the panel exactly as `DashboardPanel::view` does — construct the bare
/// struct, kick the refresh loop, then drive its first (leading, no-sleep)
/// iteration to quiescence. `run_until_parked` returns once the loop parks on
/// its 5 s timer, i.e. after exactly one full refresh.
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
fn dashboard_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on("service.health", json!({ "ok": true }))
        .on(
            "system.inventory",
            json!({ "cpu_brand": "Test CPU", "cpu_cores": 8 }),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on("memory.long_term.list", json!({ "memories": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.initial_load_done, "the first refresh cycle completed");
            assert!(
                !panel.service_health.is_empty(),
                "the health strip was populated with the monitored services"
            );
            assert!(
                panel
                    .service_health
                    .iter()
                    .all(|r| r.health.status == HealthStatus::Healthy),
                "every monitored service reports Healthy on the happy path"
            );
        })
        .unwrap();
    // The strip probes every monitored service.
    assert!(fake.count_for("service.health") >= 1, "health was probed");
}

#[gpui::test]
fn dashboard_survives_backend_down(cx: &mut TestAppContext) {
    // Today's real scenario: the daemon is in no-spawn mode, so every service
    // read fails at the transport. The Dashboard must degrade — red dots — not
    // panic, and still finish its load cycle.
    let fake = ScriptedBackend::new()
        .on_err("service.health", "pipe_unavailable: lifecycle not running")
        .on_err(
            "system.inventory",
            "pipe_unavailable: vram-broker not running",
        )
        .on_err("ollama.list_loaded", "pipe_unavailable: ollama not running")
        .on_err(
            "memory.long_term.list",
            "pipe_unavailable: harness not running",
        );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.initial_load_done,
                "the loader still completes when every backend is down"
            );
            assert!(
                panel
                    .service_health
                    .iter()
                    .all(|r| r.health.status == HealthStatus::Unhealthy),
                "a down daemon is DETECTED as Unhealthy, not silently green"
            );
        })
        .unwrap();
}

#[gpui::test]
fn dashboard_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    // A service that answers not-ok (an error envelope) is surfaced by the pipe
    // as Err too — same graceful path, distinct cause. The strip must still
    // reflect it as unhealthy without tearing down.
    let fake = ScriptedBackend::new()
        .on_err("service.health", "internal_error: probe blew up")
        .on_err("system.inventory", "internal_error: broker blew up")
        .on_err("ollama.list_loaded", "internal_error: ollama blew up")
        .on_err("memory.long_term.list", "internal_error: harness blew up");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.initial_load_done,
                "loader completes on error envelopes"
            );
            assert!(
                panel
                    .service_health
                    .iter()
                    .all(|r| r.health.status == HealthStatus::Unhealthy),
                "an error-envelope service reads Unhealthy"
            );
        })
        .unwrap();
}

#[gpui::test]
fn dashboard_tolerates_empty_backend(cx: &mut TestAppContext) {
    // Today's finding: degraded services return ok/EMPTY, not errors. The
    // default fake answers every unscripted action with `Ok({})`; a panel that
    // assumed non-empty would break. The Dashboard must load an empty-but-clean
    // strip (services present, hardware unknown) without panic.
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.initial_load_done,
                "loader completes against empty ok-replies"
            );
            assert!(
                !panel.hardware_ever_read,
                "an empty inventory is treated as 'not read', not a bogus zero card"
            );
        })
        .unwrap();
}
