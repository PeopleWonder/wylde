//! End-to-end agentic-reasoning S3 tests: the streaming turn driver with a
//! mock `wylde-ollama` pipe serving BOTH surfaces a Deep turn touches —
//! the unary `ollama.chat` (the PLAN call) and the streaming
//! `ollama.chat_stream` (the execute rounds).
//!
//! Exercises the S3 done-when list:
//! * canned reasoner plan → the plan checklist events fire and the plan
//!   step's guidance rides the round's message tail → the tool dispatches
//!   through the existing gates;
//! * planner garbage → visible fallback notice + plain ReAct;
//! * zero-step plan → direct answer, no guidance;
//! * **identity**: with the gate closed (toggle off, or depth Fast) the
//!   full turn-event transcript AND every Ollama request body are
//!   byte-identical to a plain turn — no plan call, no `format`, no
//!   guidance message.
//!
//! Same shared-mock-pipe discipline as `run_turn_loop_e2e.rs` (Config's
//! process-wide OnceLock freezes the service name at first use).
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OnceCell};
use wylde_harness::turn::actions as chat;
use wylde_harness::turn::reasoning::ReasoningConfig;
use wylde_shared::ipc;

/// One mock pipe per test binary: unary `ollama.chat` (PLAN) + streaming
/// `ollama.chat_stream` (rounds), both programmable and both recording
/// every request body they receive.
struct Harness {
    /// Count + program + received bodies for the unary (PLAN) surface.
    unary_count: Arc<AtomicU32>,
    unary_reply: Arc<Mutex<Value>>,
    unary_bodies: Arc<Mutex<Vec<Value>>>,
    /// Per-round content script + received bodies for the stream surface.
    stream_count: Arc<AtomicU32>,
    stream_script: Arc<Mutex<Vec<String>>>,
    stream_bodies: Arc<Mutex<Vec<Value>>>,
    _server: Arc<ipc::PipeServer>,
}

async fn harness() -> &'static Harness {
    static H: OnceCell<Harness> = OnceCell::const_new();
    H.get_or_init(|| async {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let service = format!("ollama-mock-rsn-{suffix}");
        std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &service);
        // Hermetic data dir: the reasoning config, conversation docs and
        // memory stores all live under here for this binary.
        let data_dir = std::env::temp_dir().join(format!("wylde-rsn-e2e-{suffix}"));
        std::env::set_var("WYLDE_DATA_DIR", &data_dir);
        // Workspace reads must hit a guaranteed-dead pipe (degrade path).
        std::env::set_var("WYLDE_HARNESS_WORKSPACES_SERVICE", "wylde-ws-dead-rsn");
        // Keep the background post-turn LLM pass off the mock's counters.
        std::env::set_var("WYLDE_POST_TURN_EXTRACTION", "off");

        let unary_count = Arc::new(AtomicU32::new(0));
        let unary_reply = Arc::new(Mutex::new(json!({"message": {"content": ""}})));
        let unary_bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let count = Arc::clone(&unary_count);
            let reply = Arc::clone(&unary_reply);
            let bodies = Arc::clone(&unary_bodies);
            ipc::register_action("ollama.chat", move |payload: Value| {
                count.fetch_add(1, Ordering::SeqCst);
                bodies.lock().unwrap().push(payload);
                let r = reply.lock().unwrap().clone();
                async move { ipc::Reply::ok(r) }
            });
        }

        let stream_count = Arc::new(AtomicU32::new(0));
        let stream_script: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stream_bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let count = Arc::clone(&stream_count);
            let script = Arc::clone(&stream_script);
            let bodies = Arc::clone(&stream_bodies);
            ipc::register_streaming_action("ollama.chat_stream", move |payload: Value, sender| {
                let n = count.fetch_add(1, Ordering::SeqCst) as usize;
                bodies.lock().unwrap().push(payload);
                let content = script
                    .lock()
                    .unwrap()
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| "fell off the script".to_owned());
                async move {
                    let _ = sender
                        .send(Ok(json!({"message": {"content": content}, "done": false})))
                        .await;
                    let _ = sender
                        .send(Ok(json!({
                            "done": true,
                            "prompt_eval_count": 7,
                            "eval_count": 3,
                        })))
                        .await;
                }
            });
        }

        let server = Arc::new(ipc::PipeServer::new(&service));
        let server_clone = Arc::clone(&server);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            let _ = rt.block_on(server_clone.accept_loop());
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        Harness {
            unary_count,
            unary_reply,
            unary_bodies,
            stream_count,
            stream_script,
            stream_bodies,
            _server: server,
        }
    })
    .await
}

/// Serialise tests, reset counters/recordings, bypass the consent gate
/// (dispatch semantics are tested elsewhere).
async fn test_guard<'a>() -> (MutexGuard<'a, ()>, &'static Harness) {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    let guard = LOCK.lock().await;
    wylde_harness::tooling::consent::set_bypass_for_tests(true);
    let h = harness().await;
    h.unary_count.store(0, Ordering::SeqCst);
    h.stream_count.store(0, Ordering::SeqCst);
    h.unary_bodies.lock().unwrap().clear();
    h.stream_bodies.lock().unwrap().clear();
    (guard, h)
}

fn set_reasoning_enabled(enabled: bool) {
    let cfg = ReasoningConfig {
        enabled,
        ..ReasoningConfig::default()
    };
    ReasoningConfig::persist(cfg).expect("persist reasoning config");
}

/// Run one full streaming turn and collect its user-facing events.
async fn run_streaming_turn(payload: Value) -> Vec<Value> {
    let reply = chat::handle_start_turn(payload).await;
    assert!(reply.ok, "start_turn failed: {:?}", reply.error);
    let turn_id = reply.data["turn_id"].as_str().unwrap().to_owned();

    let (tx, mut rx) = tokio::sync::mpsc::channel(512);
    // stream_turn exits when the turn is done and drained.
    tokio::time::timeout(
        Duration::from_secs(20),
        chat::handle_stream_turn(json!({"turn_id": turn_id}), tx),
    )
    .await
    .expect("turn did not complete in time");

    let mut events = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        events.push(chunk.expect("stream chunk"));
    }
    events
}

/// A canned, valid PlanDag naming the always-registered `time.now` tool.
fn canned_plan() -> String {
    json!({
        "goal": "tell the time",
        "steps": [{
            "id": "s1",
            "intent": "get the current time",
            "tool": "time.now",
            "args_template": {},
            "depends_on": [],
            "expected": {
                "predicates": [{"kind": "non_empty"}],
                "assertion": "a timestamp is returned",
                "on_surprise": "continue",
                "confidence": 0.9
            }
        }],
        "reasoning_trace": "need the clock for this",
        "plan_version": 1
    })
    .to_string()
}

fn set_unary_content(h: &Harness, content: &str) {
    *h.unary_reply.lock().unwrap() = json!({
        "message": {"content": content},
        "prompt_eval_count": 100,
        "eval_count": 40,
    });
}

fn set_stream_script(h: &Harness, rounds: &[&str]) {
    *h.stream_script.lock().unwrap() = rounds.iter().map(|s| s.to_string()).collect();
}

/// Every recorded message content across all stream bodies, flattened.
fn all_stream_message_contents(h: &Harness) -> Vec<String> {
    h.stream_bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| {
            b["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m["content"].as_str().map(str::to_owned))
        })
        .collect()
}

fn events_of_type<'a>(events: &'a [Value], t: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["type"].as_str() == Some(t))
        .collect()
}

/// Mask `role:"tool"` message contents (the dispatched tool's own output —
/// `time_now` embeds wall-clock) so request-body comparison pins exactly
/// what the reasoning layer could have touched: roles, order, the system
/// prompt, and every model-facing message.
fn mask_tool_results(messages: &Value) -> Value {
    let mut m = messages.clone();
    if let Some(arr) = m.as_array_mut() {
        for msg in arr {
            if msg["role"] == "tool" {
                msg["content"] = json!("<tool output masked>");
            }
        }
    }
    m
}

/// Strip the per-run turn_id so two transcripts compare structurally.
fn normalised(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .map(|e| {
            let mut e = e.clone();
            if let Some(o) = e.as_object_mut() {
                o.remove("turn_id");
            }
            e
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn deep_turn_plans_grounds_and_guides_the_round() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    set_unary_content(h, &canned_plan());
    set_stream_script(
        h,
        &[
            // Round 1: the model follows the guidance and calls the tool.
            "{\"name\": \"time.now\", \"arguments\": {}}",
            // Round 2: it answers.
            "it is noon",
        ],
    );

    let events = run_streaming_turn(json!({
        "user_message": "what time is it?",
        "conversation_id": "rsn-deep-1",
        "model": "stub",
        "depth": "deep",
    }))
    .await;

    // The PLAN call happened, exactly once, on the unary surface.
    assert_eq!(h.unary_count.load(Ordering::SeqCst), 1, "one PLAN call");

    // …and it was grammar-constrained + ctx-capped + think-budgeted.
    let plan_body = h.unary_bodies.lock().unwrap()[0].clone();
    assert_eq!(
        plan_body["format"]["required"][0], "goal",
        "PLAN rides the PlanDag format schema (constrained decoding ON by default)"
    );
    assert_eq!(plan_body["options"]["num_ctx"], 32768, "reasoner ctx cap");
    assert_eq!(
        plan_body["options"]["num_predict"],
        4096 + 2048,
        "think budget + plan-JSON output allowance"
    );
    assert_eq!(plan_body["stream"], false);
    assert!(
        plan_body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("planning stage"),
        "plan system prompt in place"
    );
    assert!(
        plan_body["messages"][1]["content"]
            .as_str()
            .unwrap()
            .starts_with("### Goal\nwhat time is it?"),
        "grounded user prompt leads with the goal"
    );

    // Phase + checklist + thinking events fired.
    let phases: Vec<&str> = events_of_type(&events, "phase")
        .iter()
        .filter_map(|e| e["phase"].as_str())
        .collect();
    assert!(phases.contains(&"planning"), "phases: {phases:?}");
    let steps = events_of_type(&events, "step");
    let reasoning_steps: Vec<String> = steps
        .iter()
        .filter(|e| e["stage"] == "reasoning")
        .map(|e| e["summary"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        reasoning_steps
            .iter()
            .any(|s| s.starts_with("Grounded plan in")),
        "grounding step: {reasoning_steps:?}"
    );
    assert!(
        reasoning_steps.contains(&"s1 · get the current time".to_owned()),
        "plan checklist row: {reasoning_steps:?}"
    );
    assert!(
        reasoning_steps
            .iter()
            .any(|s| s.starts_with("Plan drafted: 1 step(s)")),
        "drafted summary: {reasoning_steps:?}"
    );
    assert!(
        events_of_type(&events, "thinking")
            .iter()
            .any(|e| e["text"].as_str().unwrap().contains("need the clock")),
        "reasoning_trace surfaced as Thinking"
    );

    // The guidance rode the ROUND's message tail (never the system slot).
    let round1 = h.stream_bodies.lock().unwrap()[0].clone();
    let msgs = round1["messages"].as_array().unwrap().clone();
    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "user");
    let guidance = last["content"].as_str().unwrap();
    // NB: the plan named `time.now`; validation canonicalises it through
    // the executor's alias map to the registry id `time_now`.
    assert!(
        guidance.contains("[plan step 1/1 — s1]") && guidance.contains("time_now"),
        "guidance on the tail: {guidance}"
    );
    assert!(
        !msgs[0]["content"].as_str().unwrap().contains("[plan step"),
        "guidance never contaminates the system message (KV prefix, R5)"
    );

    // The tool dispatched through the normal gates and the turn completed.
    let done = events_of_type(&events, "turn_complete");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["final_message"], "it is noon");

    // Honest meter: the final usage folds the PLAN call's tokens
    // (100 prompt + 40 completion) into the two rounds' (7+3 each).
    let usage = events_of_type(&events, "usage");
    let final_usage = usage
        .iter()
        .find(|e| e["done"] == true)
        .expect("final usage");
    assert_eq!(final_usage["prompt_tokens"], 100 + 7 + 7);
    assert_eq!(final_usage["completion_tokens"], 40 + 3 + 3);
}

#[tokio::test]
async fn planner_garbage_falls_back_visibly_to_plain_react() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    set_unary_content(h, "I refuse to emit JSON, sorry.");
    set_stream_script(h, &["a direct answer"]);

    let events = run_streaming_turn(json!({
        "user_message": "hello",
        "conversation_id": "rsn-garbage-1",
        "model": "stub",
        "depth": "deep",
    }))
    .await;

    // Visible notice, then the turn proceeds and completes normally.
    let notices: Vec<String> = events_of_type(&events, "step")
        .iter()
        .filter(|e| e["stage"] == "reasoning")
        .map(|e| e["summary"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        notices
            .iter()
            .any(|s| s == "Planner output invalid — running direct"),
        "visible fallback notice: {notices:?}"
    );
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "a direct answer"
    );
    // No guidance contaminated the round.
    assert!(
        !all_stream_message_contents(h)
            .iter()
            .any(|c| c.contains("[plan step")),
        "no guidance after fallback"
    );
}

#[tokio::test]
async fn zero_step_plan_answers_directly() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    set_unary_content(
        h,
        &json!({
            "goal": "greet",
            "steps": [],
            "reasoning_trace": "trivial — answer directly",
            "plan_version": 1
        })
        .to_string(),
    );
    set_stream_script(h, &["hi there"]);

    let events = run_streaming_turn(json!({
        "user_message": "hi",
        "conversation_id": "rsn-zero-1",
        "model": "stub",
        "depth": "deep",
    }))
    .await;

    let summaries: Vec<String> = events_of_type(&events, "step")
        .iter()
        .filter(|e| e["stage"] == "reasoning")
        .map(|e| e["summary"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        summaries.contains(&"Plan: answer directly (0 steps)".to_owned()),
        "{summaries:?}"
    );
    // Trace still surfaced.
    assert!(events_of_type(&events, "thinking")
        .iter()
        .any(|e| e["text"].as_str().unwrap().contains("trivial")));
    // And the round carried NO guidance.
    assert!(!all_stream_message_contents(h)
        .iter()
        .any(|c| c.contains("[plan step")));
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "hi there"
    );
}

/// THE identity proof (S3 done-when: "identity transcript test Deep-off ==
/// trunk"). Two gate-closed configurations — (a) toggle ON but depth Fast,
/// (b) explicit Deep but toggle OFF — must produce a turn-event transcript
/// and Ollama request bodies IDENTICAL to a plain baseline turn: zero PLAN
/// calls, zero `format` keys, zero guidance messages.
#[tokio::test]
async fn identity_gate_closed_transcript_matches_plain_turn() {
    let (_g, h) = test_guard().await;

    let script: &[&str] = &["{\"name\": \"time.now\", \"arguments\": {}}", "done"];
    let mut transcripts: Vec<Vec<Value>> = Vec::new();
    let mut request_bodies: Vec<Vec<Value>> = Vec::new();

    // (baseline) toggle off, no depth field — trunk behaviour.
    // (a) toggle ON, depth fast. (b) toggle OFF, depth deep.
    for (enabled, depth) in [(false, None), (true, Some("fast")), (false, Some("deep"))] {
        set_reasoning_enabled(enabled);
        set_stream_script(h, script);
        // Each run replays the SAME script from round 0.
        h.stream_count.store(0, Ordering::SeqCst);
        h.stream_bodies.lock().unwrap().clear();

        let mut payload = json!({
            "user_message": "what time is it?",
            "conversation_id": format!("rsn-id-{enabled}-{depth:?}"),
            "model": "stub",
        });
        if let Some(d) = depth {
            payload["depth"] = json!(d);
        }
        transcripts.push(normalised(&run_streaming_turn(payload).await));
        request_bodies.push(h.stream_bodies.lock().unwrap().clone());
    }

    // ZERO unary (PLAN) calls across all three runs.
    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        0,
        "gate closed ⇒ the reasoner is never called"
    );

    // Event transcripts: byte-identical to the baseline.
    assert_eq!(
        transcripts[1], transcripts[0],
        "toggle-on + Fast must be byte-identical to trunk"
    );
    assert_eq!(
        transcripts[2], transcripts[0],
        "Deep + toggle-off must be byte-identical to trunk"
    );
    // Sanity: the transcript is non-trivial (phases + steps + completion).
    assert!(transcripts[0].len() >= 4, "{:?}", transcripts[0]);

    // Request bodies: identical message arrays (same system prompt, same
    // tail, no guidance), no `format` key anywhere.
    for (i, bodies) in request_bodies.iter().enumerate().skip(1) {
        assert_eq!(
            bodies.len(),
            request_bodies[0].len(),
            "same round count as baseline (run {i})"
        );
        for (b, base) in bodies.iter().zip(&request_bodies[0]) {
            assert_eq!(
                mask_tool_results(&b["messages"]),
                mask_tool_results(&base["messages"]),
                "run {i}: request messages must match the baseline byte-for-byte \
                 (tool-result contents masked — the real time_now tool embeds \
                 wall-clock, an inherent nondeterminism unrelated to reasoning)"
            );
            assert!(b.get("format").is_none(), "no format key on a fast turn");
        }
    }
}
