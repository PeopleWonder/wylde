//! `chat.*` in-process dispatch — verb names → `HarnessApi` calls.
//!
//! Mirror of `Core/GUI/src-tauri/src/pipe/chat.rs`.  Streaming verbs
//! (`chat.stream_turn`, `chat.stream_tools`) return `None` so the
//! caller falls through to the wire path; the plan §5.5 swaps them
//! to `gpui::Subscription` when the Chat panel ports.

use serde_json::Value;
use wylde_harness::HarnessApi;
use wylde_shared::ipc::Reply;

pub async fn dispatch<A: HarnessApi + ?Sized>(
    api: &A,
    verb: &str,
    payload: Value,
) -> Option<Reply> {
    match verb {
        "chat.run_turn" => Some(api.chat_run_turn(payload).await),
        "chat.start_turn" => Some(api.chat_start_turn(payload).await),
        "chat.cancel" => Some(api.chat_cancel(payload).await),
        "chat.stream_turn" | "chat.stream_tools" => None,
        _ => None,
    }
}
