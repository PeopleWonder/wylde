//! End-to-end `chat.complete` tests with a mock `wylde-ollama` pipe
//! (Wylde_Study S2a).
//!
//! `chat.complete` is the narrow single-shot completion verb extensions
//! use. These tests pin its real round-trip through the shared
//! `ollama.chat` IPC action — the same pipeline `chat.run_turn` uses —
//! against a synthetic backend, asserting:
//!
//! * the cleaned response text, resolved `model_used`, and summed
//!   `tokens_used` (plus the prompt/completion breakdown) surface; and
//! * exactly one `{role: "user"}` message is sent with NO system prompt
//!   and NO `tools` field (the deliberately narrow surface), and
//!   `max_tokens` is forwarded as Ollama `options.num_predict`.
//!
//! ## Why a dedicated mock pipe
//!
//! `Config` caches `ollama_service` in a process-wide `OnceLock` on the
//! first `chat.*` call, so we set `WYLDE_HARNESS_OLLAMA_SERVICE` before
//! any handler runs and serve a mock under that name for the whole
//! binary. The mock records the last payload it saw so the message-shape
//! assertions can inspect it.
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OnceCell};
use wylde_harness::turn::actions as chat;
use wylde_shared::ipc;

struct Harness {
    /// The last payload the mock `ollama.chat` action received.
    last_payload: Arc<AsyncMutex<Option<Value>>>,
    _server: Arc<ipc::PipeServer>,
}

async fn harness() -> &'static Harness {
    static H: OnceCell<Harness> = OnceCell::const_new();
    H.get_or_init(|| async {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let service = format!("ollama-complete-mock-{suffix}");
        std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &service);

        let last_payload: Arc<AsyncMutex<Option<Value>>> = Arc::new(AsyncMutex::new(None));
        let last_for_handler = Arc::clone(&last_payload);
        ipc::register_action("ollama.chat", move |payload: Value| {
            let last = Arc::clone(&last_for_handler);
            async move {
                *last.lock().await = Some(payload);
                // Mirror Ollama's /api/chat (stream=false) reply: the
                // assistant message plus token counters + the echoed model.
                ipc::Reply::ok(json!({
                    "model": "mock-llm",
                    "message": {"role": "assistant", "content": "the answer is 42"},
                    "prompt_eval_count": 11,
                    "eval_count": 7,
                    "done": true,
                }))
            }
        });

        let server = Arc::new(ipc::PipeServer::new(&service));
        let server_clone = Arc::clone(&server);
        // Run the server on a dedicated OS thread + runtime so it
        // survives across every `#[tokio::test]` in this binary.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            let _ = rt.block_on(server_clone.accept_loop());
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        Harness {
            last_payload,
            _server: server,
        }
    })
    .await
}

async fn test_guard<'a>() -> (MutexGuard<'a, ()>, &'static Harness) {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    let guard = LOCK.lock().await;
    let h = harness().await;
    (guard, h)
}

#[tokio::test]
async fn complete_returns_text_model_and_token_counts() {
    let (_g, _h) = test_guard().await;

    let reply = chat::handle_complete(json!({
        "prompt": "what is the answer?",
        "model": "mock-llm",
    }))
    .await;

    assert!(reply.ok, "expected ok reply, got {reply:?}");
    assert_eq!(reply.data["text"], "the answer is 42");
    assert_eq!(reply.data["model_used"], "mock-llm");
    // tokens_used = prompt_eval_count + eval_count.
    assert_eq!(reply.data["tokens_used"], 18);
    assert_eq!(reply.data["prompt_tokens"], 11);
    assert_eq!(reply.data["completion_tokens"], 7);
}

#[tokio::test]
async fn complete_sends_single_user_message_with_no_system_or_tools() {
    let (_g, h) = test_guard().await;

    let _ = chat::handle_complete(json!({
        "prompt": "narrow surface check",
        "model": "mock-llm",
        "max_tokens": 64,
    }))
    .await;

    let payload = h
        .last_payload
        .lock()
        .await
        .clone()
        .expect("mock saw a payload");

    let messages = payload["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1, "exactly one message: {messages:?}");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "narrow surface check");
    // The narrow contract: no system prompt, no tool catalog.
    assert!(
        messages.iter().all(|m| m["role"] != "system"),
        "chat.complete must not inject a system message"
    );
    assert!(
        payload.get("tools").is_none(),
        "chat.complete must not advertise tools"
    );
    // max_tokens maps onto Ollama's generation option.
    assert_eq!(payload["options"]["num_predict"], 64);
}

#[tokio::test]
async fn complete_rejects_missing_prompt_without_calling_backend() {
    let (_g, _h) = test_guard().await;
    let reply = chat::handle_complete(json!({"model": "mock-llm"})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}
