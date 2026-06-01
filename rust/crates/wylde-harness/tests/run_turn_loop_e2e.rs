//! End-to-end `chat.run_turn` tests with a mock `wylde-ollama` pipe.
//!
//! Phase 5.C exercises:
//!
//! * `MAX_TOOL_LOOPS` enforcement — a synthetic Ollama that keeps
//!   emitting tool-call JSON bails out with `tool_loop_limit`.
//! * `tool_calls_summary` populated in the reply.
//! * MCP-route dispatch (`webcrawler.*`) surfaces the bridge's reply
//!   in the summary as a successful call row.
//! * Internal-route dispatch returns `not_implemented` from the stub
//!   but the tool is still recorded as a summary row with `ok: false`.
//!
//! ## Why one shared mock pipe
//!
//! `Config` is cached in a process-wide `OnceLock`, so the first test
//! to invoke [`chat::handle_run_turn`] freezes the `ollama_service`
//! pipe name. All tests in this binary share a single mock with a
//! switchable reply table. Tests serialise via a mutex so the table
//! flip is observable in order.
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OnceCell};
use wylde_harness::turn::actions as chat;
use wylde_shared::ipc;

/// One mock-pipe + handler set per test binary. Holds the round
/// counter and the per-test reply-program slot.
struct Harness {
    counter: Arc<AtomicU32>,
    program: Arc<AsyncMutex<ReplyProgram>>,
    _server: Arc<ipc::PipeServer>,
}

type ReplyProgram = Box<dyn FnMut(u32) -> Reply + Send + 'static>;

enum Reply {
    /// Plain assistant content — model returns text and no tool calls.
    Content(String),
    /// Bare-JSON tool call in content — the salvage parser picks it
    /// up; the alias map drives recognised vs unrecognised.
    ToolCall { name: String, args: Value },
}

impl Reply {
    fn to_ollama_reply(&self) -> Value {
        match self {
            Reply::Content(s) => json!({"message": {"content": s}}),
            Reply::ToolCall { name, args } => json!({
                "message": {
                    "content": format!(
                        "{{\"name\": \"{name}\", \"arguments\": {args}}}",
                        args = serde_json::to_string(args).unwrap()
                    )
                }
            }),
        }
    }
}

async fn harness() -> &'static Harness {
    static H: OnceCell<Harness> = OnceCell::const_new();
    H.get_or_init(|| async {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let service = format!("ollama-mock-{suffix}");
        std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &service);

        let counter = Arc::new(AtomicU32::new(0));
        let program: Arc<AsyncMutex<ReplyProgram>> = Arc::new(AsyncMutex::new(Box::new(
            |_n: u32| Reply::Content("default".into()),
        )));

        let counter_for_handler = Arc::clone(&counter);
        let program_for_handler = Arc::clone(&program);
        ipc::register_action("ollama.chat", move |_payload: Value| {
            let n = counter_for_handler.fetch_add(1, Ordering::SeqCst);
            let program = Arc::clone(&program_for_handler);
            async move {
                let mut p = program.lock().await;
                let reply = (p)(n);
                ipc::Reply::ok(reply.to_ollama_reply())
            }
        });

        let server = Arc::new(ipc::PipeServer::new(&service));
        let server_clone = Arc::clone(&server);
        // `#[tokio::test]` spins up a fresh runtime per test and drops
        // any tasks the test spawned. Run the server on a dedicated OS
        // thread with its own runtime so it survives across all tests
        // in this binary.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            let _ = rt.block_on(server_clone.accept_loop());
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        Harness {
            counter,
            program,
            _server: server,
        }
    })
    .await
}

/// Serialise tests so the reply-program flip + round-counter inspect
/// pair stays observable. Also flips the Phase-12.2 consent bypass
/// on — every test in this file exercises a real tool dispatch and
/// we want the gate out of the way (gate semantics are tested in
/// `tooling::runner::tests` and `tooling::consent::tests`). This is a
/// distinct cargo-test binary from those, so no cross-binary mutex
/// is needed: the bypass is set once and stays set for the process.
async fn test_guard<'a>() -> (MutexGuard<'a, ()>, &'static Harness) {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    let guard = LOCK.lock().await;
    wylde_harness::tooling::consent::set_bypass_for_tests(true);
    let h = harness().await;
    h.counter.store(0, Ordering::SeqCst);
    (guard, h)
}

async fn set_program<F>(h: &Harness, f: F)
where
    F: FnMut(u32) -> Reply + Send + 'static,
{
    *h.program.lock().await = Box::new(f);
}

#[tokio::test]
async fn run_turn_returns_natural_completion_for_no_tool_calls() {
    let (_g, h) = test_guard().await;
    set_program(h, |_n| Reply::Content("all done".into())).await;

    let reply = chat::handle_run_turn(json!({
        "user_message": "hi",
        "conversation_id": "c1",
        "model": "stub",
    }))
    .await;

    assert!(reply.ok, "expected ok reply, got {reply:?}");
    assert_eq!(reply.data["final_message"], "all done");
    assert_eq!(reply.data["aborted"], false);
    assert_eq!(reply.data["abort_reason"], Value::Null);
    let summary = reply.data["tool_calls_summary"]
        .as_array()
        .expect("array");
    assert!(summary.is_empty(), "no tool calls → empty summary");
    assert_eq!(
        h.counter.load(Ordering::SeqCst),
        1,
        "exactly one ollama.chat round"
    );
}

#[tokio::test]
async fn run_turn_emits_unrecognised_tool_error_when_alias_missing() {
    // The 5.C salvage parser passes an empty alias map (Phase 6 builds
    // a real one alongside the tool registry). So a model-emitted
    // `fs.read` lands in `unrecognised` → tool_error event, NO summary
    // row, AND no follow-up round (only recovered calls drive another
    // iteration). The salvage parser scrubs the JSON, leaving an
    // empty final_message. Mirrors Python's `_driver.py:354-368`.
    let (_g, h) = test_guard().await;
    set_program(h, |_n| Reply::ToolCall {
        name: "fs.read".into(),
        args: json!({"path": "x"}),
    })
    .await;

    let reply = chat::handle_run_turn(json!({
        "user_message": "read x",
        "conversation_id": "c1",
        "model": "stub",
    }))
    .await;

    assert!(reply.ok);
    // Salvage scrubbed the tool-call JSON; nothing else to render.
    assert_eq!(reply.data["final_message"], "");
    assert_eq!(reply.data["aborted"], false);
    let summary = reply.data["tool_calls_summary"]
        .as_array()
        .expect("array");
    assert!(
        summary.is_empty(),
        "unrecognised salvage call must not produce a summary row, got {summary:?}"
    );
    // One ollama round — the unrecognised path does NOT loop again.
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_turn_dispatches_internal_tool_through_registry_then_completes() {
    // Phase 6 end-to-end: the model emits a `time.now` tool call, the
    // salvage parser resolves it via the alias map (registry-populated),
    // dispatch routes Internal → registry → active handler. The reply
    // shows ONE tool_calls_summary row with ok: true. The second round
    // returns plain content so the loop exits naturally.
    let (_g, h) = test_guard().await;
    set_program(h, |n| {
        if n == 0 {
            Reply::ToolCall {
                name: "time.now".into(),
                args: json!({}),
            }
        } else {
            Reply::Content("done with the time".into())
        }
    })
    .await;

    let reply = chat::handle_run_turn(json!({
        "user_message": "what time is it?",
        "conversation_id": "c1",
        "model": "stub",
    }))
    .await;

    assert!(reply.ok, "expected ok reply, got {reply:?}");
    assert_eq!(reply.data["final_message"], "done with the time");
    assert_eq!(reply.data["aborted"], false);
    let summary = reply.data["tool_calls_summary"]
        .as_array()
        .expect("array");
    assert_eq!(summary.len(), 1, "exactly one tool call recorded");
    assert_eq!(summary[0]["ok"], true, "internal tool succeeded");
    // Salvage normalises to the canonical registry id via the alias map.
    assert_eq!(summary[0]["name"], "time_now");
    // Two ollama rounds: one for the tool-call emission, one for the
    // post-tool natural-completion reply.
    assert_eq!(h.counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn run_turn_records_deferred_tool_as_summary_row_with_error_reason() {
    // `git.git_status` is a non-destructive Phase-6-deferred stub
    // (sandbox-spawn decision pending). The model emits it, dispatch
    // resolves it, the registry returns phase_6_deferred, and the
    // summary row carries ok: false plus the error code. Picked a
    // non-destructive entry so the tier gate (default `tool_use`)
    // doesn't short-circuit before the deferred check.
    //
    // Slice 11.E (2026-05-26) flipped the voice.transcribe /
    // voice.synthesize tools to active, so this test now exercises a
    // shell-sandbox deferred tool instead.
    let (_g, h) = test_guard().await;
    set_program(h, |n| {
        if n == 0 {
            Reply::ToolCall {
                name: "git.git_status".into(),
                args: json!({}),
            }
        } else {
            Reply::Content("git status deferred".into())
        }
    })
    .await;

    let reply = chat::handle_run_turn(json!({
        "user_message": "show git status",
        "conversation_id": "c1",
        "model": "stub",
    }))
    .await;

    assert!(reply.ok);
    let summary = reply.data["tool_calls_summary"].as_array().unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["ok"], false);
    assert!(summary[0]["error"]
        .as_str()
        .unwrap()
        .contains("phase_6_deferred"));
}

#[tokio::test]
async fn run_turn_blocks_destructive_tool_call_on_tool_use_tier() {
    // fs.write_file is registered as destructive. On tool_use tier
    // (the default), the tier gate denies before the handler runs.
    let (_g, h) = test_guard().await;
    set_program(h, |n| {
        if n == 0 {
            Reply::ToolCall {
                name: "fs.write_file".into(),
                args: json!({"path": "out.txt", "content": "hi"}),
            }
        } else {
            Reply::Content("ok aborted".into())
        }
    })
    .await;

    let reply = chat::handle_run_turn(json!({
        "user_message": "write a file",
        "conversation_id": "c1",
        "model": "stub",
        "device_tier": "tool_use",
    }))
    .await;

    assert!(reply.ok);
    let summary = reply.data["tool_calls_summary"].as_array().unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["ok"], false);
    assert_eq!(summary[0]["reason"], "tier_read_only");
    assert!(summary[0]["error"]
        .as_str()
        .unwrap()
        .contains("destructive"));
}

#[tokio::test]
async fn run_turn_returns_completion_shape_with_all_expected_fields() {
    let (_g, h) = test_guard().await;
    set_program(h, |_n| Reply::Content("ok".into())).await;

    let reply = chat::handle_run_turn(json!({
        "user_message": "hi",
        "conversation_id": "conv-42",
        "turn_id": "turn-1234",
        "model": "stub",
    }))
    .await;

    assert!(reply.ok);
    assert_eq!(reply.data["turn_id"], "turn-1234");
    assert_eq!(reply.data["conversation_id"], "conv-42");
    assert_eq!(reply.data["final_message"], "ok");
    assert!(reply.data["tool_calls_summary"].is_array());
    assert_eq!(reply.data["aborted"], false);
    assert_eq!(reply.data["abort_reason"], Value::Null);
    assert_eq!(reply.data["abort_error"], Value::Null);
}
