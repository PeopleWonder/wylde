//! L7 **control**-walk — Memory (issue #247).
//!
//! `panel_walk.rs` proves this panel loads; this proves its controls work.
//! The harness is `wylde_gui_test_support::control_walk` — see its module docs
//! for the oracle, states, and the `add_window` requirement.
//!
//! Adding a control here needs no edit to this file: build it with
//! `control(div(), "id")` and it is walked automatically.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_memory::MemoryPanel;

/// Memory's observable surface: what `panel_walk.rs` asserts on, plus the
/// expanded set a row click toggles in-frame and the copy-in status line.
fn fingerprint(p: &MemoryPanel) -> String {
    format!(
        "lt={} ws={} st={} expanded={:?} err={:?} loading={}/{}/{} feedback={:?}",
        p.long_term.len(),
        p.workspaces.len(),
        p.short_term.len(),
        p.expanded,
        p.error,
        p.loading_long_term,
        p.loading_workspaces,
        p.loading_short_term,
        p.copy_feedback,
    )
}

/// A record plus a workspace, so both the row toggle and the per-record
/// copy-in button paint.
fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "memory.long_term.list",
            json!({ "memories": [
                { "id": "m1", "body": "prefers terse answers", "importance": 9,
                  "source": "reflection", "created_at": 0.0, "last_used_at": 0.0, "tags": [] },
            ]}),
        )
        .on(
            "workspaces.list_mru",
            json!({ "workspaces": [{ "id": "ws-a", "name": "ws-a", "path": "/tmp/a" }] }),
        )
        .on("notes.add", json!({ "ok": true }))
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<MemoryPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = MemoryPanel::new(cx);
        MemoryPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<MemoryPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        // The copy-in button only paints on an EXPANDED row, so the default
        // frame never shows it. Without this state it would be silently
        // uncovered — the exact false-coverage shape the harness guards.
        .state("row-expanded", |p: &mut MemoryPanel, _w, cx| {
            p.expanded.insert("m1".to_string());
            cx.notify();
        })
        .sources(&[include_str!("../src/memory_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_memory_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_walk_covers_the_controls_memory_renders(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    for expected in ["memory-refresh", "memory-row::m1"] {
        assert!(
            painted.contains(&expected),
            "{expected} painted and was walked; got {painted:?}"
        );
    }
    // The copy-in button carries a RUNTIME id, so the literal-id guard cannot
    // see it — this is the assertion that proves the `row-expanded` state is
    // doing real work rather than being decorative. Delete the state and this
    // fails, which is the point.
    assert!(
        painted.iter().any(|id| id.starts_with("memory-copyin::")),
        "the copy-in button paints only on an expanded row, and the          `row-expanded` state must reach it; got {painted:?}"
    );
}

/// A click that drives the panel into the error branch `panel_walk` never
/// repaints. Refresh is the way out of a broken state and must not be dead.
#[gpui::test]
fn controls_survive_being_clicked_in_the_error_branch(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on_err("memory.long_term.list", "pipe_unavailable: harness down")
        .on_err("workspaces.list_mru", "pipe_unavailable: harness down");
    let _guard = fake.clone().install();
    let window = mount(cx);

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.error.is_some(), "the fixture really is the error branch");
        })
        .unwrap();

    walk(cx, window, &fake).assert_every_control_lives();
}
