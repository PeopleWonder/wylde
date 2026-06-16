//! Windowed gpui tests for the docked/global ChatPanel's conversation
//! lifecycle and the global-vs-workspace isolation contract.
//!
//! These extend the dock-scoping suite (`tests/dock_scoping.rs`) past list
//! scoping into the conversation CRUD a user actually drives — create / select
//! / delete — plus the long-term-memory confinement that a turn's binding
//! decides. They mount a real `ChatPanel` in a gpui test window and drive it
//! through the scripted fake backend at the `wylde_gui_pipe::call` seam,
//! asserting observable state AND the exact verbs+payloads issued.
//!
//! What they retire (owed feel-tests):
//!   (a) "+ New" on a dock INSIDE a workspace mints a *bound* thread
//!       (conversations.set_workspace) and parks it as that workspace's
//!       last-open; on the Global slot it mints an *unbound* thread and never
//!       binds (D1) — the create-side of global-vs-workspace isolation.
//!   (b) selecting a thread persists the *right* pointer for the surface
//!       (per-workspace on a bound dock, global on the Global slot — C7/D1).
//!   (c) deleting the active thread inside a workspace removes it and falls
//!       back to the next remaining thread, persisting the new selection.
//!   (d) the delete-confirmation arm/cancel lifecycle.
//!   (e) long-term confinement: a *bound* turn carries the workspace_id (so the
//!       harness excludes long-term — [D2]); a *global* turn carries none (so
//!       the harness includes long-term).
//!
//! Determinism: every async effect is driven to quiescence with
//! `cx.run_until_parked()` before asserting. See `docs/gui-testing.md`.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope};

/// One newest-first conversation row, as `conversations.list` emits it.
/// `workspace_id` empty == unbound (a global thread).
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

/// Mount a docked dock, install `fake` (with `rows` as its scoped list), and
/// ENTER `ws`. After this the dock is inside `ws` with its conversation list
/// scoped (C4) and its open thread chosen by the C6 enter flow. Returns the
/// window AND the install guard — keep the guard alive for the test body
/// (dropping it clears the thread-local fake). The caller keeps its own `fake`
/// clone (same Arc) for assertions.
fn docked_in_workspace(
    cx: &mut TestAppContext,
    ws: &str,
    rows: Vec<Value>,
    fake: std::sync::Arc<ScriptedBackend>,
) -> (gpui::WindowHandle<ChatPanel>, wylde_gui_test_support::BackendGuard) {
    let guard = fake.conversations(rows).install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some(ws.to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();
    (window, guard)
}

// ── (a) create: bound dock binds, Global never binds (isolation) ─────────

#[gpui::test]
fn new_conversation_on_a_bound_dock_binds_to_the_workspace(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .conversations(vec![conv("c-existing", "ws-a", 200)])
        .on("conversations.new", json!({ "id": "c-new" }));
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();

    // "+ New" inside the workspace.
    window
        .update(cx, |_panel, _w, cx| ChatPanel::spawn_new_conversation(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.conversation_id, "c-new", "the dock switches to the new thread");
        })
        .unwrap();

    // It bound the fresh thread to the entered workspace (write-side of D2/C5)…
    let bind = fake
        .last_call_for("conversations.set_workspace")
        .expect("a new thread on a bound dock must bind to the workspace");
    assert_eq!(bind.payload_str("id").as_deref(), Some("c-new"));
    assert_eq!(bind.payload_str("workspace_id").as_deref(), Some("ws-a"));

    // …and parked it as the workspace's last-open (C7), NOT the global pointer.
    let pointer = fake
        .last_call_for("conversations.set_active_for_workspace")
        .expect("a new bound thread becomes the workspace's last-open");
    assert_eq!(pointer.payload_str("workspace_id").as_deref(), Some("ws-a"));
    assert_eq!(pointer.payload_str("id").as_deref(), Some("c-new"));
    assert_eq!(
        fake.count_for("conversations.set_active"),
        0,
        "a bound thread must not touch the GLOBAL active pointer (D1)"
    );
}

#[gpui::test]
fn new_conversation_on_the_global_slot_never_binds(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("conversations.new", json!({ "id": "c-new" }));
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    window
        .update(cx, |_panel, _w, cx| ChatPanel::spawn_new_conversation(cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.conversation_id, "c-new");
        })
        .unwrap();

    assert_eq!(
        fake.count_for("conversations.set_workspace"),
        0,
        "the Global slot is structurally workspace-free — it never binds a thread (D1)"
    );
    // It persists on the GLOBAL pointer instead.
    let pointer = fake
        .last_call_for("conversations.set_active")
        .expect("a global new thread updates the global active pointer");
    assert_eq!(pointer.payload_str("id").as_deref(), Some("c-new"));
    assert_eq!(
        fake.count_for("conversations.set_active_for_workspace"),
        0,
        "the Global slot never writes a per-workspace pointer"
    );
}

// ── (b) select persists the right pointer per surface (C7 / D1) ──────────

#[gpui::test]
fn selecting_a_thread_on_a_bound_dock_persists_the_workspace_pointer(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let (window, _guard) = docked_in_workspace(
        cx,
        "ws-a",
        vec![conv("c1", "ws-a", 300), conv("c2", "ws-a", 100)],
        fake.clone(),
    );

    // Enter opened the most-recent (c1); now select the older one.
    window
        .update(cx, |panel, window, cx| {
            panel.select_conversation("c2", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| assert_eq!(panel.conversation_id, "c2"))
        .unwrap();

    let pointer = fake
        .last_call_for("conversations.set_active_for_workspace")
        .expect("selecting on a bound dock updates the per-workspace pointer (C7)");
    assert_eq!(pointer.payload_str("workspace_id").as_deref(), Some("ws-a"));
    assert_eq!(pointer.payload_str("id").as_deref(), Some("c2"));
    assert_eq!(
        fake.count_for("conversations.set_active"),
        0,
        "a bound selection never touches the global pointer (D1)"
    );
}

#[gpui::test]
fn selecting_a_thread_on_the_global_slot_persists_the_global_pointer(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, window, cx| {
            panel.select_conversation("c-global", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let pointer = fake
        .last_call_for("conversations.set_active")
        .expect("selecting on the Global slot updates the global pointer");
    assert_eq!(pointer.payload_str("id").as_deref(), Some("c-global"));
    assert_eq!(
        fake.count_for("conversations.set_active_for_workspace"),
        0,
        "the Global slot never writes a per-workspace pointer (D1)"
    );
}

// ── (c) delete the active thread → fall back + persist ───────────────────

#[gpui::test]
fn deleting_the_active_thread_falls_back_to_the_next(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("conversations.delete", json!({ "ok": true }));
    // Enter ws-a with two bound threads; the enter flow opens the newest (c1).
    let (window, _guard) = docked_in_workspace(
        cx,
        "ws-a",
        vec![conv("c1", "ws-a", 300), conv("c2", "ws-a", 100)],
        fake.clone(),
    );
    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.conversation_id, "c1", "enter opened the most-recent thread");
            assert_eq!(panel.conversations.len(), 2);
        })
        .unwrap();

    // Delete the active thread. The drop + fallback are SYNCHRONOUS (the
    // delete/reload IPC is spawned after), so assert the optimistic UI state
    // inside the same update — before the async tail runs. (The reload would
    // re-fetch the *static* scripted list here, which a real backend, having
    // actually deleted c1, would not; the optimistic state is the user-visible
    // behavior under test.)
    window
        .update(cx, |panel, _w, cx| {
            panel.confirm_delete_conversation("c1", cx);
            assert_eq!(
                panel.conversation_id, "c2",
                "deleting the active thread falls back to the next remaining one"
            );
            assert!(
                !panel.conversations.iter().any(|c| c.id == "c1"),
                "the deleted row is dropped from the rail (optimistic)"
            );
            assert!(panel.confirm_delete.is_none(), "the confirmation is cleared");
        })
        .unwrap();
    cx.run_until_parked();

    // The async tail fired the delete + persisted the fallback selection.
    let del = fake
        .last_call_for("conversations.delete")
        .expect("confirm must fire conversations.delete");
    assert_eq!(del.payload_str("id").as_deref(), Some("c1"));
    let set = fake
        .last_call_for("conversations.set_active")
        .expect("the fallback selection is persisted (so a restart restores it)");
    assert_eq!(set.payload_str("id").as_deref(), Some("c2"));
}

// ── (d) delete-confirmation arm / cancel lifecycle ───────────────────────

#[gpui::test]
fn delete_confirmation_arms_and_cancels_without_deleting(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, cx| {
            panel.request_delete_conversation("c1", cx);
            assert_eq!(panel.confirm_delete.as_deref(), Some("c1"), "arm targets the row");
            // A different target replaces the first (only one live confirm).
            panel.request_delete_conversation("c2", cx);
            assert_eq!(panel.confirm_delete.as_deref(), Some("c2"));
            panel.cancel_delete_conversation(cx);
            assert!(panel.confirm_delete.is_none(), "cancel dismisses the confirm");
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        fake.count_for("conversations.delete"),
        0,
        "arming + cancelling must never delete anything"
    );
}

// ── (e) long-term confinement is decided by a turn's binding ([D2]) ──────

#[gpui::test]
fn a_bound_turn_carries_the_workspace_excluding_long_term(cx: &mut TestAppContext) {
    // A docked turn inside a workspace carries the workspace_id; the harness's
    // context_gather reads exactly that to CONFINE long-term memory out of a
    // bound turn ([D2]: bound conversation excludes the global long-term store).
    let fake = ScriptedBackend::new()
        .on("chat.start_turn", json!({ "turn_id": "t1", "conversation_id": "c1" }));
    let (window, _guard) =
        docked_in_workspace(cx, "ws-a", vec![conv("c1", "ws-a", 300)], fake.clone());

    window
        .update(cx, |panel, _w, cx| {
            panel.send_user_message("what did we decide?".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    let start = fake
        .last_call_for("chat.start_turn")
        .expect("the dock send must hit chat.start_turn");
    assert_eq!(
        start.workspace_id().as_deref(),
        Some("ws-a"),
        "a bound turn carries workspace_id → harness excludes long-term ([D2])"
    );
}

#[gpui::test]
fn a_global_turn_carries_no_workspace_including_long_term(cx: &mut TestAppContext) {
    // The Global slot's turn is structurally workspace-free, so context_gather
    // INCLUDES the long-term store (the unbound path) — the mirror of [D2].
    let fake = ScriptedBackend::new()
        .on("chat.start_turn", json!({ "turn_id": "t1", "conversation_id": "default" }));
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, cx| {
            panel.send_user_message("who am I?".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    let start = fake
        .last_call_for("chat.start_turn")
        .expect("the global send must hit chat.start_turn");
    assert_eq!(
        start.workspace_id(),
        None,
        "a global turn carries no workspace_id → harness includes long-term (mirror of [D2])"
    );
}

// ── (f) a failed delete is surfaced, not swallowed (GUI-responsiveness) ───

#[gpui::test]
fn a_failed_delete_surfaces_an_error(cx: &mut TestAppContext) {
    // GUI-responsiveness pass (category c): the row is dropped optimistically,
    // so a swallowed delete failure would vanish-then-flicker with no
    // explanation. The failure must reach the error strip.
    let fake = ScriptedBackend::new().on_err("conversations.delete", "pipe_unavailable: harness down");
    let (window, _guard) = docked_in_workspace(
        cx,
        "ws-a",
        vec![conv("c1", "ws-a", 300), conv("c2", "ws-a", 100)],
        fake.clone(),
    );

    window
        .update(cx, |panel, _w, cx| panel.confirm_delete_conversation("c1", cx))
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            let e = panel.error.as_deref().unwrap_or_default();
            assert!(
                e.contains("delete conversation"),
                "a failed delete is surfaced, not swallowed: {e:?}"
            );
        })
        .unwrap();
}
