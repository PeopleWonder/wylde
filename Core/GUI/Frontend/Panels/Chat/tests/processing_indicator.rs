//! Windowed gpui tests for the live chat-processing indicator
//! (chat-processing-indicator).
//!
//! They mount a real `ChatPanel` in a gpui test window and drive it through
//! the scripted fake backend at the `wylde_gui_pipe::call` / `stream_call`
//! seam, exercising:
//!   * the expand/collapse toggle on the live indicator, and
//!   * a full streamed turn (phase → usage → token → complete) settling its
//!     activity log + token meter onto the finished assistant bubble, with the
//!     live `processing` state cleared (which also stops the animation ticker).
//!
//! Determinism: every async effect is driven to quiescence with
//! `cx.run_until_parked()` before asserting. See `docs/gui-testing.md`.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope, MessageRole};
use wylde_panel_chat::processing::{ProcessingPhase, ProcessingState};

#[gpui::test]
fn live_indicator_expand_collapse_toggles(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, cx| {
            // Stand up an in-flight processing state with detail to expand.
            let mut p = ProcessingState::new();
            p.set_phase(ProcessingPhase::GatheringContext);
            p.on_tool_dispatched("c1", "memory.search");
            p.on_usage(Some(40), 18);
            panel.processing = Some(p);

            // Default collapsed.
            assert!(!panel.processing.as_ref().unwrap().expanded);
            // Click expands…
            panel.toggle_processing_expanded(cx);
            assert!(panel.processing.as_ref().unwrap().expanded);
            // …and click collapses.
            panel.toggle_processing_expanded(cx);
            assert!(!panel.processing.as_ref().unwrap().expanded);
        })
        .unwrap();
}

#[gpui::test]
fn a_streamed_turn_drives_phases_then_settles_activity_onto_the_bubble(
    cx: &mut TestAppContext,
) {
    // Idle → animating → phases → tokens → done, end-to-end through the real
    // stream pump. The user-facing stream replays the chunks the harness emits.
    let fake = ScriptedBackend::new()
        .on(
            "chat.start_turn",
            json!({ "turn_id": "t1", "conversation_id": "default" }),
        )
        .on_stream(
            "chat.stream_turn",
            vec![
                json!({"type": "phase", "turn_id": "t1", "phase": "gathering_context"}),
                json!({"type": "phase", "turn_id": "t1", "phase": "generating"}),
                json!({"type": "usage", "turn_id": "t1", "completion_tokens": 120, "done": false}),
                json!({"type": "usage", "turn_id": "t1", "prompt_tokens": 1000, "completion_tokens": 200, "done": true}),
                json!({"type": "token", "turn_id": "t1", "text": "Hello!"}),
                json!({"type": "turn_complete", "turn_id": "t1", "final_message": "Hello!"}),
            ],
        )
        .on_stream("chat.stream_tools", vec![]);
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, cx| {
            panel.send_user_message("hi".to_owned(), cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |panel, _w, _cx| {
            // The turn settled: live processing cleared (ticker stops with it).
            assert!(
                panel.processing.is_none(),
                "processing must clear when the turn completes"
            );

            let assistant = panel
                .messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::Assistant)
                .expect("an assistant bubble exists");
            assert!(!assistant.streaming, "the bubble is no longer streaming");
            assert_eq!(assistant.content, "Hello!");

            // The activity (token meter at least) folded onto the bubble as a
            // persistent disclosure — 1000 + 200 = 1.2k tokens, no tools ran.
            let act = assistant
                .activity
                .as_ref()
                .expect("activity folds onto the settled bubble");
            assert!(!act.is_empty());
            assert_eq!(act.summary(), "1.2k tokens");
            // Collapsed by default.
            assert!(!assistant.activity_expanded);
        })
        .unwrap();
}
