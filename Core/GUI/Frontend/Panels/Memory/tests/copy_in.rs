//! Windowed gpui tests for the Memory panel's long-term browser and the
//! "Copy to «workspace»" promotion (C2b copy-in).
//!
//! Mount a real `MemoryPanel` in a gpui test window, drive it through the
//! scripted fake backend at the `wylde_gui_pipe::call` seam, and assert both
//! observable state (the long-term rows; the copy target; the feedback strip)
//! AND the exact verb+payload the copy-in issues.
//!
//! What they retire (owed feel-tests):
//!   (a) the long-term section loads its importance-sorted rows from
//!       `memory.long_term.list`.
//!   (b) the copy target is the MRU-head workspace ("entered" ≈ most recent).
//!   (c) "Copy to «workspace»" fires `workspaces.notes.add` with the record's
//!       body, the target workspace, and the `long-term-copy` provenance tag —
//!       the manual promotion path that keeps long-term OUT of bound workspace
//!       chats ([D2]) — and reports success in the feedback strip.
//!   (d) a copy failure soft-fails into the feedback strip (no teardown).
//!
//! Determinism: every async effect is driven to quiescence with
//! `cx.run_until_parked()` before asserting. See `docs/gui-testing.md`.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_memory::ipc::COPY_IN_SOURCE;
use wylde_panel_memory::MemoryPanel;

/// One `memory.long_term.list` record row.
fn record(id: &str, body: &str, importance: i32) -> Value {
    json!({
        "id": id,
        "body": body,
        "source": "reflection",
        "importance": importance,
        "created_at": 0.0,
        "last_used_at": 0.0,
        "tags": [],
    })
}

/// One `workspaces.list_mru` row (MRU order — row 0 is most recent).
fn ws_row(id: &str, folder: &str) -> Value {
    json!({ "id": id, "folder": folder })
}

/// Mount + first-load the panel (mirrors the factory's `view()` kick of
/// `spawn_refresh`).
fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<MemoryPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = MemoryPanel::new(cx);
        MemoryPanel::spawn_refresh(cx);
        panel
    });
    cx.run_until_parked();
    window
}

// ── (a) the long-term rows load ──────────────────────────────────────────

#[gpui::test]
fn long_term_rows_load_from_the_list_verb(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "memory.long_term.list",
        json!({ "memories": [
            record("m1", "prefers terse answers", 9),
            record("m2", "based in Australia", 6),
        ]}),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, _cx| {
            let ids: Vec<&str> = panel.long_term.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["m1", "m2"],
                "the long-term section lists the curated rows"
            );
            assert_eq!(panel.long_term[0].body, "prefers terse answers");
            assert_eq!(panel.long_term[0].importance, 9);
            assert!(!panel.search_active, "a plain list load is not a search");
            assert!(
                !panel.loading_long_term,
                "the load flag clears once rows arrive"
            );
        })
        .unwrap();
    assert_eq!(fake.count_for("memory.long_term.list"), 1);
}

// ── (b) the copy target is the MRU head ──────────────────────────────────

#[gpui::test]
fn copy_target_is_the_mru_head_workspace(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "workspaces.list_mru",
        json!({ "workspaces": [ws_row("ws-recent", "C:/r"), ws_row("ws-old", "C:/o")] }),
    );
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.copy_target_id().as_deref(),
                Some("ws-recent"),
                "copy-in targets the most-recently-activated (MRU head) workspace"
            );
        })
        .unwrap();
}

// ── (c) "Copy to «workspace»" → notes.add with provenance ────────────────

#[gpui::test]
fn copy_to_workspace_issues_notes_add_with_provenance(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "workspaces": [ws_row("ws-recent", "C:/r")] }),
        )
        .on(
            "memory.long_term.list",
            json!({ "memories": [record("m1", "uses tabs not spaces", 8)] }),
        )
        // The notes.add reply shape doesn't matter; the panel only cares it's Ok.
        .on("workspaces.notes.add", json!({ "ok": true }));
    let _guard = fake.clone().install();

    let window = mount(cx);

    // Copy the record's body into the (MRU-head) target workspace.
    window
        .update(cx, |panel, _w, cx| {
            let target = panel
                .copy_target_id()
                .expect("a workspace exists to copy into");
            let body = panel.long_term[0].body.clone();
            panel.spawn_copy_in(target, body, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let add = fake
        .last_call_for("workspaces.notes.add")
        .expect("copy-in must fire workspaces.notes.add");
    assert_eq!(
        add.service, "wylde-workspaces",
        "notes live on the workspaces service"
    );
    assert_eq!(
        add.payload_str("workspace_id").as_deref(),
        Some("ws-recent")
    );
    assert_eq!(
        add.payload_str("text").as_deref(),
        Some("uses tabs not spaces"),
        "the record's BODY is what gets copied in"
    );
    assert_eq!(
        add.payload_str("source").as_deref(),
        Some(COPY_IN_SOURCE),
        "the note is tagged with the long-term-copy provenance so it's auditable"
    );

    window
        .update(cx, |panel, _w, _cx| {
            let msg = panel.copy_feedback.as_deref().unwrap_or_default();
            assert!(
                msg.contains("Copied"),
                "a success strip confirms the copy: {msg:?}"
            );
        })
        .unwrap();
}

// ── (d) a copy failure soft-fails into the feedback strip ────────────────

#[gpui::test]
fn copy_failure_surfaces_in_the_feedback_strip(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err(
        "workspaces.notes.add",
        "pipe_unavailable: workspaces service not running",
    );
    let _guard = fake.clone().install();

    let window = mount(cx);
    window
        .update(cx, |panel, _w, cx| {
            panel.spawn_copy_in("ws-x".to_owned(), "some body".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            let msg = panel.copy_feedback.as_deref().unwrap_or_default();
            assert!(
                msg.contains("failed"),
                "a transport failure surfaces in the strip, not a teardown: {msg:?}"
            );
        })
        .unwrap();
    assert_eq!(fake.count_for("workspaces.notes.add"), 1);
}
