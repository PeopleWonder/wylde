//! L7 panel-walk — Tools (issue #35, roadmap T0.1b).
//!
//! One of the four zero-coverage panels. Mount the real `ToolsPanel` the way
//! the Shell does (`new()` + `spawn_refresh`), drive the extension-bridge IPC
//! through the scripted fake, and assert it survives every realistic backend
//! condition.
//!
//! **What "error state" means for Tools:** a panel-level `error: Option<String>`
//! set when `ext.list` fails, plus a `loading: bool` that must clear once the
//! first reply lands (a panel stuck `loading == true` is the "stuck spinner"
//! failure). The gate: `error.is_none()` + `!loading` on the happy path;
//! `error.is_some()` (surfaced, not swallowed) when the bridge is down.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_tools::ipc::PanelAvailability;
use wylde_panel_tools::ToolsPanel;

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ToolsPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = ToolsPanel::new();
        ToolsPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

#[gpui::test]
fn tools_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "ext.list",
            json!({ "extensions": [
                { "id": "ext-a", "name": "Ext A", "version": "1.0.0", "enabled": true },
            ]}),
        )
        .on("extensions.list_panels", json!({ "panels": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert!(
                !panel.loading,
                "the loading spinner clears once the list arrives"
            );
            assert_eq!(panel.extensions.len(), 1, "the extension row loaded");
        })
        .unwrap();
    assert_eq!(fake.count_for("ext.list"), 1);
}

#[gpui::test]
fn tools_survives_backend_down(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("ext.list", "pipe_unavailable: extension-bridge not running")
        .on_err(
            "extensions.list_panels",
            "pipe_unavailable: extension-bridge not running",
        );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "a down bridge surfaces a visible error — not a silent empty list"
            );
            assert!(
                !panel.loading,
                "the spinner still clears on failure (no stuck spinner)"
            );
        })
        .unwrap();
}

#[gpui::test]
fn tools_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err("ext.list", "internal_error: bridge blew up");
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_some(),
                "an error envelope surfaces on the panel"
            );
            assert!(!panel.loading);
        })
        .unwrap();
}

#[gpui::test]
fn tools_tolerates_empty_backend(cx: &mut TestAppContext) {
    // Default fake → every action returns Ok({}); `ext.list` parses to an empty
    // extension list. Empty is NOT an error here.
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "empty ok-replies are not an error");
            assert!(!panel.loading);
            assert!(
                panel.extensions.is_empty(),
                "an empty bridge yields an empty list"
            );
        })
        .unwrap();
}

// ────────────────────────────────────────────────────────────────────
// #239 — "no silent dead panel".
//
// The GUI is a projection of live discovery + live health. These pin the
// two halves at the surface the user actually looks at:
//
//   * a registration the bridge no longer reports yields no card; and
//   * a registration it DOES report but cannot reach renders with its
//     unavailable status, never as a card that looks like it works.
//
// The bridge recomputes both per read (`Host::list_panels`), so these
// hold for every extension with no per-extension wiring — the property
// that makes the mechanism self-extending. `wylde-images` appears as the
// worked example because it is the stub that forced the issue; nothing
// here is specific to it.
// ────────────────────────────────────────────────────────────────────

/// The `extensions.list_panels` reply for a panel-only extension whose
/// service is gone — the shape the bridge produces for a stub pointing
/// at a dead port.
fn dead_images_panel() -> serde_json::Value {
    json!({ "panels": [{
        "extension": "wylde-images",
        "id": "images",
        "title": "Images",
        "icon": "image",
        "kind": "iframe",
        "url": "http://127.0.0.1:8015",
        "availability": "unreachable",
        "detail": "nothing is listening at its address"
    }]})
}

#[gpui::test]
fn a_registration_the_bridge_no_longer_reports_yields_no_card(cx: &mut TestAppContext) {
    // The extension is gone from disk, so the bridge's re-walked catalog
    // no longer mentions it. The panel must hold nothing to render — not
    // a stale card carried over from an earlier read.
    let fake = ScriptedBackend::new()
        .on("ext.list", json!({ "extensions": [] }))
        .on("extensions.list_panels", json!({ "panels": [] }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.panels.is_empty(),
                "a registration that is no longer reported must leave no card behind"
            );
            assert!(panel.error.is_none(), "an empty catalog is not an error");
        })
        .unwrap();
}

#[gpui::test]
fn an_unreachable_panel_renders_its_status_not_a_live_card(cx: &mut TestAppContext) {
    // Registered but dead: the card must exist (the user needs to know it
    // is there and broken) and must never read as live.
    let fake = ScriptedBackend::new()
        .on(
            "ext.list",
            json!({ "extensions": [
                { "name": "wylde-images", "version": "1.0", "enabled": false, "status": "disabled" },
            ]}),
        )
        .on("extensions.list_panels", dead_images_panel());
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.panels.len(), 1, "the dead panel is still listed");
            let p = &panel.panels[0];
            assert_eq!(
                p.availability,
                PanelAvailability::Unreachable,
                "a panel pointing at a dead port is unavailable"
            );
            assert!(
                !p.availability.is_live(),
                "it must never be presented as a live panel — the point of #239"
            );
            assert!(
                p.detail.is_some(),
                "the card carries a reason, not just a bare URL"
            );
            assert!(
                !p.availability.label().is_empty(),
                "every card shows status or affords action"
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_reachable_panel_is_the_only_thing_that_reads_as_live(cx: &mut TestAppContext) {
    // The other side of the gate, so the assertion above can't pass by
    // the panel simply never reporting anything as live.
    let fake = ScriptedBackend::new().on(
        "extensions.list_panels",
        json!({ "panels": [
            {
                "extension": "n8n", "id": "workflows", "title": "Workflows",
                "kind": "iframe", "url": "http://127.0.0.1:5678",
                "availability": "live"
            },
            {
                "extension": "wylde-images", "id": "images", "title": "Images",
                "kind": "iframe", "url": "http://127.0.0.1:8015",
                "availability": "unreachable",
                "detail": "nothing is listening at its address"
            },
        ]}),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            let live: Vec<&str> = panel
                .panels
                .iter()
                .filter(|p| p.availability.is_live())
                .map(|p| p.id.as_str())
                .collect();
            assert_eq!(
                live,
                vec!["workflows"],
                "exactly the reachable panel reads as live"
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_service_going_away_lands_on_the_next_read_without_a_restart(cx: &mut TestAppContext) {
    // Reacting at runtime is the requirement, not just being right once.
    // A second read must replace the first rather than be ignored.
    let fake = ScriptedBackend::new().on(
        "extensions.list_panels",
        json!({ "panels": [{
            "extension": "n8n", "id": "workflows", "title": "Workflows",
            "kind": "iframe", "url": "http://127.0.0.1:5678",
            "availability": "live"
        }]}),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.panels[0].availability.is_live(), "live to begin with");
        })
        .unwrap();

    // The service dies: the bridge's next reply reports it unreachable.
    fake.clone()
        .on("extensions.list_panels", dead_images_panel());
    window
        .update(cx, |_panel, _w, cx| ToolsPanel::spawn_refresh(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.panels.len(), 1);
            assert!(
                !panel.panels[0].availability.is_live(),
                "the card flips to unavailable on the next read — no restart, \
                 no manual file surgery"
            );
        })
        .unwrap();
}
