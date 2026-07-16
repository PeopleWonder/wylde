//! Windowed gpui tests for the Chat **type-and-send** control — the
//! critical-path control #35 names as "send message".
//!
//! Scope note, because it is easy to duplicate coverage here: the
//! `chat.start_turn` *dispatch* is already covered (`conversations.rs`,
//! `dock_scoping.rs`) by calling `send_user_message` directly. What those
//! never touch is everything **upstream** of it — the composer wiring a user
//! actually drives. So these tests enter at the real seam: set text on
//! `prompt_input` and emit `InputEvent::Submit`, exactly as pressing Enter
//! does (`SubmitMode::EnterSubmits`), and let the panel's own subscription
//! route it into `submit_text` → `send_user_message`.
//!
//! That path owns three behaviours nothing else asserts, and all three are
//! guards whose failure is silent and user-visible:
//!   * the typed text reaches the turn,
//!   * the composer is cleared afterwards (or the user re-sends their text),
//!   * empty sends and double-sends are blocked (`submit_text`'s guard —
//!     the duplicate-turn bug `starting` exists to prevent).

use gpui::TestAppContext;
use serde_json::json;

use wylde_gpui_input::InputEvent;
use wylde_gui_test_support::{BackendGuard, ScriptedBackend};
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope};

/// Mount a global dock with `fake` installed. Global scope keeps these tests
/// about the composer rather than workspace binding (covered elsewhere).
fn mount(
    cx: &mut TestAppContext,
    fake: std::sync::Arc<ScriptedBackend>,
) -> (gpui::WindowHandle<ChatPanel>, BackendGuard) {
    let guard = fake.conversations(vec![]).install();
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Global, cx));
    cx.run_until_parked();
    (window, guard)
}

/// Type `text` into the composer and press Enter — the real user gesture.
fn type_and_press_enter(
    cx: &mut TestAppContext,
    window: &gpui::WindowHandle<ChatPanel>,
    text: &str,
) {
    window
        .update(cx, |panel, _w, cx| {
            panel.prompt_input.update(cx, |input, cx| {
                input.set_text(text, cx);
                cx.emit(InputEvent::Submit(input.text().to_owned()));
            });
        })
        .unwrap();
    cx.run_until_parked();
}

#[gpui::test]
fn pressing_enter_sends_the_typed_text_and_clears_the_composer(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "chat.start_turn",
        json!({ "turn_id": "t1", "conversation_id": "c1" }),
    );
    let (window, _guard) = mount(cx, fake.clone());

    type_and_press_enter(cx, &window, "what did we decide about the slug?");

    let call = fake
        .last_call_for("chat.start_turn")
        .expect("pressing Enter in the composer must start a turn");
    assert_eq!(
        call.payload_str("user_message").as_deref(),
        Some("what did we decide about the slug?"),
        "the turn carries the text the user actually typed"
    );
    window
        .update(cx, |panel, _w, cx| {
            assert_eq!(
                panel.prompt_input.read(cx).text(),
                "",
                "the composer is cleared once the panel takes the message — \
                 leaving it would let the user re-send the same text"
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_whitespace_only_submit_starts_no_turn(cx: &mut TestAppContext) {
    // `submit_text` trims before its is_empty check. An accidental Enter on an
    // empty composer must not burn a turn (or an LLM call).
    let fake = ScriptedBackend::new().on(
        "chat.start_turn",
        json!({ "turn_id": "t1", "conversation_id": "c1" }),
    );
    let (window, _guard) = mount(cx, fake.clone());

    type_and_press_enter(cx, &window, "   \t  ");

    assert_eq!(
        fake.count_for("chat.start_turn"),
        0,
        "a whitespace-only Enter must not start a turn"
    );
}

#[gpui::test]
fn a_second_enter_while_a_turn_is_starting_does_not_double_send(cx: &mut TestAppContext) {
    // The regression `starting` exists for: between Enter and `start_turn`
    // returning, `active_turn_id` is still None, so a second Enter would slip
    // past that guard and start a DUPLICATE turn. Drive exactly that window by
    // submitting twice before letting the backend reply.
    let fake = ScriptedBackend::new().on(
        "chat.start_turn",
        json!({ "turn_id": "t1", "conversation_id": "c1" }),
    );
    let (window, _guard) = mount(cx, fake.clone());

    window
        .update(cx, |panel, _w, cx| {
            panel.prompt_input.update(cx, |input, cx| {
                input.set_text("hello", cx);
                cx.emit(InputEvent::Submit(input.text().to_owned()));
            });
            // Second Enter in the same update — before `start_turn` can settle,
            // so `starting` is the only thing standing between us and a dupe.
            panel.prompt_input.update(cx, |input, cx| {
                input.set_text("hello", cx);
                cx.emit(InputEvent::Submit(input.text().to_owned()));
            });
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        fake.count_for("chat.start_turn"),
        1,
        "a double Enter must start exactly one turn, not two"
    );
}
