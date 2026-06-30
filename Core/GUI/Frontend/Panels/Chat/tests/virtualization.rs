//! Windowed gpui tests for the **virtualized** message log.
//!
//! The message log renders through gpui's [`gpui::list`] so only the bubbles in
//! (and just around) the viewport are built each frame — render cost is bounded
//! regardless of how long the conversation is. These tests pin the two halves of
//! that contract:
//!
//!   * the reconciler ([`ChatPanel::sync_message_list`]) keeps the [`ListState`]
//!     item count in lock-step with `messages` across the mutations a real
//!     session drives — append a turn, grow the streaming bubble in place,
//!     switch/clear the whole thread — and preserves the right scroll/follow
//!     behaviour at each (stick-to-bottom while streaming; don't yank a
//!     scrolled-up reader; snap a freshly-loaded thread to its newest message);
//!   * an actual paint over a constrained viewport at large N builds only a
//!     bounded slice of the items, not all of them (the perf win), while still
//!     including the tail (follow-mode stick-to-bottom).
//!
//! These drive the reconciler directly via `sync_message_list` rather than
//! leaning on paint scheduling, so they're deterministic. The lone paint test
//! uses [`VisualTestContext::draw`] to exercise the real `list` layout. See
//! `docs/gui-testing.md`.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    div, list, point, px, size, AppContext, Context, Entity, FollowMode, IntoElement,
    ParentElement, Render, Styled, TestAppContext, VisualTestContext, Window,
};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatMessage, ChatPanel, ChatScope, MessageRole};

/// Build a bare message with explicit fields (the panel's own constructors are
/// private; the struct fields are public for exactly this).
fn msg(id: &str, role: MessageRole, content: &str, streaming: bool) -> ChatMessage {
    ChatMessage {
        id: id.to_owned(),
        role,
        content: content.to_owned(),
        thinking: None,
        streaming,
        activity: None,
        activity_expanded: false,
    }
}

/// Mount a Global Chat slot over an (otherwise empty) scripted backend, drain
/// its mount-time network setup, then clear any conversation it auto-loaded so
/// the test controls `messages` exactly. Returns the window + the install guard
/// (keep it alive for the test body).
fn mount(
    cx: &mut TestAppContext,
) -> (
    gpui::WindowHandle<ChatPanel>,
    wylde_gui_test_support::BackendGuard,
) {
    let guard = ScriptedBackend::new().install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, panel_cx| {
            panel.messages.clear();
            panel.sync_message_list();
            panel_cx.notify();
        })
        .unwrap();
    cx.run_until_parked();
    (window, guard)
}

// ── reconciler: item count tracks `messages` ─────────────────────────────

#[gpui::test]
fn list_item_count_tracks_messages(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.message_list.item_count(), 0, "starts empty");

            panel.messages.push(msg("u1", MessageRole::User, "hi", false));
            panel
                .messages
                .push(msg("a1", MessageRole::Assistant, "hello", false));
            panel.sync_message_list();
            assert_eq!(
                panel.message_list.item_count(),
                panel.messages.len(),
                "the list knows exactly the messages that exist"
            );
        })
        .unwrap();
}

// ── reconciler: a new turn appends and sticks to the bottom ──────────────

#[gpui::test]
fn appending_a_turn_keeps_following_the_tail(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            // Seed an existing thread and reconcile.
            for i in 0..3 {
                panel.messages.push(msg(
                    &format!("m{i}"),
                    MessageRole::Assistant,
                    "old",
                    false,
                ));
            }
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 3);

            // Append within the SAME thread (head unchanged): item count grows by
            // the delta and the list keeps following the tail (stick-to-bottom).
            panel.messages.push(msg("m3", MessageRole::User, "new", false));
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 4, "spliced the new tail");
            assert!(
                panel.message_list.is_following_tail(),
                "appending a new message stays pinned to the bottom"
            );
        })
        .unwrap();
}

// ── reconciler: appending while scrolled up does NOT yank to the bottom ───

#[gpui::test]
fn appending_while_scrolled_up_preserves_the_reader(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            for i in 0..5 {
                panel.messages.push(msg(
                    &format!("m{i}"),
                    MessageRole::Assistant,
                    "line",
                    false,
                ));
            }
            panel.sync_message_list();

            // Simulate the reader scrolling up to read history: follow disengages.
            panel.message_list.set_follow_mode(FollowMode::Normal);
            assert!(!panel.message_list.is_following_tail());

            // A same-thread append must NOT silently re-engage tail-follow — the
            // splice path leaves the follow state (and thus the scroll) alone.
            panel
                .messages
                .push(msg("m5", MessageRole::System, "info", false));
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 6);
            assert!(
                !panel.message_list.is_following_tail(),
                "a background append leaves a scrolled-up reader where they are"
            );
        })
        .unwrap();
}

// ── reconciler: streaming grows the tail in place (count stable) ──────────

#[gpui::test]
fn streaming_growth_keeps_count_stable_and_follows(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            panel
                .messages
                .push(msg("u1", MessageRole::User, "question", false));
            panel
                .messages
                .push(msg("a1", MessageRole::Assistant, "", true));
            panel.message_list.set_follow_mode(FollowMode::Tail);
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 2);

            // Tokens accumulate on the in-flight assistant bubble: the item COUNT
            // never changes (the bubble grows in place), and the list keeps
            // following the tail so the growing reply stays on screen.
            for tok in ["Hello", " there", " — here", " is more text"] {
                panel.messages.last_mut().unwrap().content.push_str(tok);
                panel.sync_message_list();
                assert_eq!(
                    panel.message_list.item_count(),
                    2,
                    "streaming grows the bubble, not the item count"
                );
                assert!(panel.message_list.is_following_tail());
            }

            // Turn completes: streaming flips off; still two items, still pinned.
            panel.messages.last_mut().unwrap().streaming = false;
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 2);
        })
        .unwrap();
}

// ── reconciler: switching threads rebuilds and snaps to the newest ───────

#[gpui::test]
fn switching_thread_resets_and_reengages_tail(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            // Thread A, then the reader scrolls up (follow off).
            for i in 0..4 {
                panel.messages.push(msg(
                    &format!("a{i}"),
                    MessageRole::Assistant,
                    "A",
                    false,
                ));
            }
            panel.sync_message_list();
            panel.message_list.set_follow_mode(FollowMode::Normal);
            assert!(!panel.message_list.is_following_tail());

            // Switch to a DIFFERENT thread (new head id, different length):
            // wholesale replace → reset to the new count AND re-engage tail so
            // the loaded thread opens pinned to its most recent message.
            panel.messages = vec![
                msg("b0", MessageRole::User, "B-one", false),
                msg("b1", MessageRole::Assistant, "B-two", false),
            ];
            panel.sync_message_list();
            assert_eq!(
                panel.message_list.item_count(),
                2,
                "the list rebuilt to the switched thread's length"
            );
            assert!(
                panel.message_list.is_following_tail(),
                "a freshly loaded thread opens at its newest message"
            );
        })
        .unwrap();
}

// ── reconciler: clearing empties the list ────────────────────────────────

#[gpui::test]
fn clearing_messages_empties_the_list(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            panel
                .messages
                .push(msg("u1", MessageRole::User, "x", false));
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 1);

            panel.messages.clear();
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 0, "cleared to empty");
        })
        .unwrap();
}

// ── send re-engages tail even from a scrolled-up position ─────────────────

#[gpui::test]
fn send_user_message_reengages_tail_follow(cx: &mut TestAppContext) {
    let guard = ScriptedBackend::new()
        .on(
            "chat.start_turn",
            serde_json::json!({ "turn_id": "t1", "conversation_id": "default" }),
        )
        .install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            // Reader was scrolled up reading history.
            panel.message_list.set_follow_mode(FollowMode::Normal);
            assert!(!panel.message_list.is_following_tail());
        })
        .unwrap();

    window
        .update(cx, |panel, _w, panel_cx| {
            panel.send_user_message("a brand new question".to_owned(), panel_cx);
            // send pushes the user+assistant bubbles and re-engages tail-follow
            // synchronously, before the async turn round-trip.
            assert!(
                panel.message_list.is_following_tail(),
                "starting a turn snaps back to the bottom"
            );
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), panel.messages.len());
        })
        .unwrap();

    cx.run_until_parked();
    drop(guard);
}

// ── smoke: the reconciler scales to a long log ───────────────────────────

#[gpui::test]
fn reconciler_scales_to_a_long_log(cx: &mut TestAppContext) {
    let (window, _guard) = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            for i in 0..1_000 {
                let role = if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                };
                panel
                    .messages
                    .push(msg(&format!("m{i}"), role, "some message body", false));
            }
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), 1_000);
            assert!(panel.message_list.is_following_tail());
        })
        .unwrap();
}

// ── paint: only a bounded slice of a long log is actually built ──────────

#[gpui::test]
fn paint_builds_only_the_visible_slice(cx: &mut TestAppContext) {
    const N: usize = 1_000;
    const ITEM_H: f32 = 40.0;
    const VIEWPORT_H: f32 = 600.0;

    let (window, _guard) = mount(cx);

    // Seed a long log and reconcile the list (follows the tail).
    window
        .update(cx, |panel, _w, _cx| {
            for i in 0..N {
                panel.messages.push(msg(
                    &format!("m{i}"),
                    MessageRole::Assistant,
                    "body",
                    false,
                ));
            }
            panel.sync_message_list();
            assert_eq!(panel.message_list.item_count(), N);
        })
        .unwrap();

    let entity: Entity<ChatPanel> = window.root(cx).unwrap();
    let state = window
        .update(cx, |panel, _w, _cx| panel.message_list.clone())
        .unwrap();

    // Record which item indices the list asks us to build during a real paint.
    let built: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));

    // The list must paint inside a real view (gpui reads the rendering view off
    // an internal stack), so wrap it in a tiny probe view that renders the
    // panel's own `ListState` and reads bubbles back out of the live ChatPanel.
    struct ProbeView {
        state: gpui::ListState,
        panel: Entity<ChatPanel>,
        built: Rc<RefCell<Vec<usize>>>,
    }
    impl Render for ProbeView {
        fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let panel = self.panel.clone();
            let built = self.built.clone();
            list(self.state.clone(), move |ix, _w, app| {
                built.borrow_mut().push(ix);
                // Mirror the production closure: read the bubble by index.
                let _alive = panel.read(app).messages.get(ix).is_some();
                div().h(px(ITEM_H)).child("•").into_any_element()
            })
            .h_full()
        }
    }

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let state_for_view = state.clone();
    let entity_for_view = entity.clone();
    let built_for_view = built.clone();
    vcx.draw(
        point(px(0.0), px(0.0)),
        size(px(400.0), px(VIEWPORT_H)),
        |_window, app| {
            app.new(|_cx| ProbeView {
                state: state_for_view.clone(),
                panel: entity_for_view.clone(),
                built: built_for_view.clone(),
            })
            .into_any_element()
        },
    );

    let built = built.borrow();
    assert!(!built.is_empty(), "the list painted some items");
    // The whole point: a 600px viewport over 40px rows builds ~15 visible plus a
    // little overdraw — a bounded slice, NOT all 1000. Generous ceiling.
    assert!(
        built.len() < 100,
        "virtualized: built {} of {N} items (bounded), not the whole log",
        built.len()
    );
    // Tail-follow means the bottom of the log is what's on screen.
    assert!(
        built.contains(&(N - 1)),
        "the newest message is among the painted slice (stuck to bottom)"
    );
    assert!(
        !built.contains(&0),
        "the oldest message is far off-screen and was never built"
    );
}
