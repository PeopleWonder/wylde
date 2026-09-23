//! Windowed gpui tests for the docked ChatPanel's workspace scoping.
//!
//! These are the first *real* windowed tests of GUI behavior — they mount an
//! actual `ChatPanel` view in a gpui test window and drive it through the
//! enter/leave + turn-send flows, asserting on observable panel state. The
//! backend is a scripted fake ([`wylde_gui_test_support::ScriptedBackend`])
//! plugged into the `wylde_gui_pipe::call` chokepoint, so no live stack runs.
//!
//! What they retire (owed feel-tests):
//!   (a) enter a workspace → the dock's conversation list scopes to that
//!       workspace's threads; leaving restores the full (unscoped) list.
//!   (b) a docked turn-send carries the entered `workspace_id`.
//!   (c) the Global Chat panel never carries a `workspace_id` (D1).
//!   (d) C6 empty-state enter: none / has-threads / has-last-open.
//!
//! Determinism: every async effect is driven to quiescence with
//! `cx.run_until_parked()` before asserting. See `docs/gui-testing.md` to add
//! more.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope};

/// One newest-first conversation row, as the harness's `conversations.list`
/// emits it. `workspace_id` empty == unbound (a global thread).
fn conv(id: &str, workspace_id: &str, updated_at: i64) -> Value {
    json!({
        "id": id,
        "title": id,
        "created_at": 0,
        "updated_at": updated_at,
        "message_count": 0,
        "working_memory_count": 0,
        "model": "",
        "workspace_id": workspace_id,
    })
}

// ── (a) enter → scoped list, leave → restore ─────────────────────────────

#[gpui::test]
fn docked_enter_scopes_conversation_list_and_leave_restores(cx: &mut TestAppContext) {
    // Full mixed list: two ws-a threads, one ws-b, one unbound.
    let fake = ScriptedBackend::new().conversations(vec![
        conv("c-a1", "ws-a", 300),
        conv("c-b1", "ws-b", 250),
        conv("c-a2", "ws-a", 200),
        conv("c-free", "", 100),
    ]);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();

    // Enter ws-a: the dock re-scopes (the path the workspace-scope bus drives).
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            let ids: Vec<&str> = panel.conversations.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["c-a1", "c-a2"],
                "dock should show ONLY the entered workspace's threads (C4)"
            );
            assert!(
                panel.conversations.iter().all(|c| c.workspace_id == "ws-a"),
                "no foreign or unbound thread may leak into a scoped rail"
            );
            assert_eq!(panel.active_workspace_id.as_deref(), Some("ws-a"));
        })
        .unwrap();

    // Leave (the back-arrow / Registry restore): the unscoped full list returns.
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(None, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.conversations.len(),
                4,
                "leaving restores the full (unscoped) conversation list"
            );
            assert_eq!(
                panel.active_workspace_id, None,
                "leaving clears the dock's workspace scope"
            );
            assert_eq!(
                panel.conversation_id, "default",
                "leaving returns the dock to its unbound default thread"
            );
        })
        .unwrap();
}

// ── (b) docked turn-send carries the entered workspace_id ────────────────

#[gpui::test]
fn docked_turn_send_carries_workspace_id(cx: &mut TestAppContext) {
    // Empty scoped list → the enter flow mints a fresh fileless thread and
    // parks the bind for the first send (C6 empty state).
    let fake = ScriptedBackend::new()
        .conversations(vec![])
        .on("conversations.new", json!({ "id": "c-fresh" }))
        .on(
            "chat.start_turn",
            json!({ "turn_id": "t1", "conversation_id": "c-fresh" }),
        );
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-beta".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    // Precondition: the empty state minted a fresh thread with a deferred bind.
    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.conversation_id, "c-fresh");
            assert_eq!(panel.pending_bind_workspace.as_deref(), Some("ws-beta"));
        })
        .unwrap();

    window
        .update(cx, |panel, _w, cx| {
            panel.send_user_message("hello".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    let start = fake
        .last_call_for("chat.start_turn")
        .expect("the dock send must hit chat.start_turn");
    assert_eq!(
        start.workspace_id().as_deref(),
        Some("ws-beta"),
        "a docked turn must carry the entered workspace_id (A1/D1)"
    );

    // And the deferred empty-state bind fired on first send (C6).
    let bind = fake
        .last_call_for("conversations.set_workspace")
        .expect("first send out of the empty state binds the thread to the workspace");
    assert_eq!(bind.payload_str("workspace_id").as_deref(), Some("ws-beta"));
}

// ── (c) the Global Chat panel is structurally workspace-free (D1) ─────────

#[gpui::test]
fn global_panel_never_carries_workspace_id(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "chat.start_turn",
        json!({ "turn_id": "t1", "conversation_id": "default" }),
    );
    let _guard = fake.clone().install();

    // Invariant the workspace picker is hidden on (render reads this).
    assert!(!ChatScope::Global.allows_workspace_bind());

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    // Even if a scope change reaches a Global panel, it must refuse to adopt it.
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.active_workspace_id, None,
                "Global must stay unbound regardless of an incoming scope (D1)"
            );
        })
        .unwrap();

    window
        .update(cx, |panel, _w, cx| {
            panel.send_user_message("hi".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    let start = fake
        .last_call_for("chat.start_turn")
        .expect("the global send must hit chat.start_turn");
    assert_eq!(
        start.workspace_id(),
        None,
        "a Global turn must NEVER ride a workspace_id (D1)"
    );
}

// ── (d) C6 empty-state enter cases ───────────────────────────────────────

#[gpui::test]
fn enter_empty_workspace_mints_fresh_thread_with_deferred_bind(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .conversations(vec![]) // none
        .on("conversations.new", json!({ "id": "c-fresh" }));
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.conversation_id, "c-fresh",
                "none → fresh fileless thread"
            );
            assert_eq!(
                panel.pending_bind_workspace.as_deref(),
                Some("ws-a"),
                "none → bind deferred to the first send"
            );
            assert!(panel.conversations.is_empty());
        })
        .unwrap();
}

#[gpui::test]
fn enter_workspace_with_threads_opens_most_recent(cx: &mut TestAppContext) {
    // Newest-first scoped list; no last-open pointer → open the head.
    let fake = ScriptedBackend::new()
        .conversations(vec![conv("c-new", "ws-a", 300), conv("c-old", "ws-a", 100)]);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.conversation_id, "c-new",
                "has-threads → open the most-recent (head of the newest-first list)"
            );
            assert_eq!(
                panel.pending_bind_workspace, None,
                "opening an existing thread carries no deferred bind"
            );
        })
        .unwrap();
}

#[gpui::test]
fn enter_workspace_honours_last_open_pointer(cx: &mut TestAppContext) {
    // A last-open pointer naming a thread still present → open THAT one, not
    // the head.
    let fake = ScriptedBackend::new()
        .conversations(vec![conv("c-new", "ws-a", 300), conv("c-old", "ws-a", 100)])
        .on(
            "conversations.get_active_for_workspace",
            json!({ "id": "c-old" }),
        );
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(
                panel.conversation_id, "c-old",
                "has-last-open → restore the per-workspace pointer's thread (C7)"
            );
        })
        .unwrap();
}
