//! L7 **control**-walk — Dashboard (issue #247).
//!
//! Harness: `wylde_gui_test_support::control_walk`. Adding a control here
//! needs no edit to this file — build it with `control(div(), "id")`.
//!
//! **Note on mounting.** Dashboard has only `spawn_refresh_loop`, no one-shot
//! variant. That is safe here: gpui's test executor drives a *simulated*
//! clock, so the loop runs its leading iteration and then parks on a timer
//! that never fires unless a test calls `advance_clock`. The walk therefore
//! still owns every backend call it counts.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_dashboard::DashboardPanel;

/// Dashboard degrades per card rather than holding one error, so the
/// fingerprint is the per-card content plus the refresh generation a Refresh
/// click bumps.
fn fingerprint(p: &DashboardPanel) -> String {
    format!(
        "svc={} models={} recent={} gen={} loaded={} hw_read={}",
        p.service_health.len(),
        p.loaded_models.len(),
        p.recent_memories.len(),
        p.refresh_generation,
        p.initial_load_done,
        p.hardware_ever_read,
    )
}

/// A service row and a memory row, so the per-service chip / stop control and
/// the per-memory row actually paint.
fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on("service.health", json!({ "ok": true }))
        .on(
            "system.inventory",
            json!({ "cpu_brand": "Test CPU", "cpu_cores": 8 }),
        )
        .on("ollama.list_loaded", json!({ "models": [] }))
        .on(
            "memory.long_term.list",
            json!({ "memories": [
                { "id": "m1", "body": "a memory", "importance": 5,
                  "source": "reflection", "created_at": 0.0, "last_used_at": 0.0, "tags": [] },
            ]}),
        )
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<DashboardPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = DashboardPanel::new();
        DashboardPanel::spawn_refresh_loop(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<DashboardPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/dashboard_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_dashboard_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_walk_covers_the_controls_dashboard_renders(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    assert!(
        painted.contains(&"dashboard-refresh"),
        "the Refresh control painted and was walked; got {painted:?}"
    );
}

/// Dashboard's whole design is per-card degradation, so the branch where every
/// service read fails is a real user state — and the one `panel_walk` cannot
/// repaint after a click. Refresh must still work there.
#[gpui::test]
fn controls_survive_being_clicked_when_every_service_is_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("service.health", "pipe_unavailable: lifecycle down")
        .on_err("system.inventory", "pipe_unavailable: broker down")
        .on_err("ollama.list_loaded", "pipe_unavailable: ollama down")
        .on_err("memory.long_term.list", "pipe_unavailable: harness down");
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake).assert_every_control_lives();
}
