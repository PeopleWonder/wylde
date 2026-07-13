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
    /// Optional per-call reply script (consumed front-first before
    /// `unary_reply` applies) — lets a test serve a different reply to a
    /// salvage retry than to the first PLAN call.
    unary_script: Arc<Mutex<Vec<Value>>>,
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
        let unary_script: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let unary_bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let count = Arc::clone(&unary_count);
            let reply = Arc::clone(&unary_reply);
            let script = Arc::clone(&unary_script);
            let bodies = Arc::clone(&unary_bodies);
            ipc::register_action("ollama.chat", move |payload: Value| {
                count.fetch_add(1, Ordering::SeqCst);
                bodies.lock().unwrap().push(payload);
                let mut s = script.lock().unwrap();
                let r = if s.is_empty() {
                    reply.lock().unwrap().clone()
                } else {
                    s.remove(0)
                };
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
            unary_script,
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
    h.unary_script.lock().unwrap().clear();
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
        "legacy \"deep\" = think_harder tier: think budget + plan-JSON output allowance"
    );
    assert!(
        plan_body.get("think").is_none(),
        "deliberating tiers OMIT the think switch (a non-thinking reasoner \
         keeps working exactly as in S3)"
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

/// The tiers slice: the `think` tier plans grammar-first with deliberation
/// OFF (`think:false`, output allowance only) and `ultrathink` carries its
/// bigger budget — the tier is a per-call knob, everything else identical.
#[tokio::test]
async fn think_and_ultrathink_tiers_set_the_call_knobs() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);

    for (depth, expect_think, expect_predict) in [
        ("think", Some(false), 2048_u64),
        ("think_harder", None, 4096 + 2048),
        ("ultrathink", None, 10240 + 2048),
    ] {
        h.unary_bodies.lock().unwrap().clear();
        set_unary_content(h, &canned_plan());
        set_stream_script(h, &["{\"name\": \"time.now\", \"arguments\": {}}", "done"]);
        h.stream_count.store(0, Ordering::SeqCst);

        let events = run_streaming_turn(json!({
            "user_message": "what time is it?",
            "conversation_id": format!("rsn-tier-{depth}"),
            "model": "stub",
            "depth": depth,
        }))
        .await;

        let plan_body = h.unary_bodies.lock().unwrap()[0].clone();
        match expect_think {
            Some(t) => assert_eq!(
                plan_body["think"],
                json!(t),
                "{depth}: the tight tier sends think:false"
            ),
            None => assert!(
                plan_body.get("think").is_none(),
                "{depth}: deliberating tiers omit the switch"
            ),
        }
        assert_eq!(
            plan_body["options"]["num_predict"],
            json!(expect_predict),
            "{depth}: tier budget + output allowance"
        );
        assert_eq!(
            plan_body["format"]["required"][0], "goal",
            "{depth}: grammar constraint always rides the PLAN call"
        );
        // The plan executed regardless of tier.
        assert_eq!(
            events_of_type(&events, "turn_complete")[0]["final_message"],
            "done"
        );
    }
}

/// Think-exhaustion salvage: a deliberating tier whose PLAN call burns the
/// whole num_predict inside `<think>` (done_reason "length", zero content)
/// gets ONE grammar-first retry with deliberation off — visible step, both
/// calls' tokens on the meter, and the salvaged plan still guides the turn.
#[tokio::test]
async fn think_exhaustion_salvages_grammar_first() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    *h.unary_script.lock().unwrap() = vec![
        json!({
            "message": {"content": "", "thinking": "hmm, let me consider every angle…"},
            "done_reason": "length",
            "prompt_eval_count": 100,
            "eval_count": 6144,
        }),
        json!({
            "message": {"content": canned_plan()},
            "done_reason": "stop",
            "prompt_eval_count": 100,
            "eval_count": 40,
        }),
    ];
    set_stream_script(h, &["{\"name\": \"time.now\", \"arguments\": {}}", "done"]);

    let events = run_streaming_turn(json!({
        "user_message": "what time is it?",
        "conversation_id": "rsn-salvage-1",
        "model": "stub",
        "depth": "think_harder",
    }))
    .await;

    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        2,
        "exactly one salvage retry"
    );
    let bodies = h.unary_bodies.lock().unwrap().clone();
    assert!(bodies[0].get("think").is_none(), "first call deliberates");
    assert_eq!(bodies[0]["options"]["num_predict"], 4096 + 2048);
    assert_eq!(
        bodies[1]["think"],
        json!(false),
        "salvage retry disables deliberation"
    );
    assert_eq!(
        bodies[1]["options"]["num_predict"], 2048,
        "salvage runs on the output allowance alone"
    );

    let notices: Vec<String> = events_of_type(&events, "step")
        .iter()
        .filter(|e| e["stage"] == "reasoning")
        .map(|e| e["summary"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        notices
            .iter()
            .any(|s| s == "Deliberation used the whole budget — retrying without it"),
        "visible salvage notice: {notices:?}"
    );
    assert!(
        notices.contains(&"s1 · get the current time".to_owned()),
        "the salvaged plan still produces the checklist: {notices:?}"
    );
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "done"
    );

    // Honest meter: BOTH plan calls' tokens fold in (100+100 prompt,
    // 6144+40 completion) plus the two rounds' 7/3 each.
    let usage = events_of_type(&events, "usage");
    let final_usage = usage
        .iter()
        .find(|e| e["done"] == true)
        .expect("final usage");
    assert_eq!(final_usage["prompt_tokens"], 200 + 7 + 7);
    assert_eq!(final_usage["completion_tokens"], 6184 + 3 + 3);
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
    // S4 extension: with the gate closed, the surprise/replan machinery is
    // never entered — no replanning phase, no surprise/replan steps, no
    // plan_precondition abort in any transcript.
    for t in &transcripts {
        for e in t {
            assert_ne!(e["phase"], json!("replanning"), "{e}");
            assert_ne!(e["reason"], json!("plan_precondition"), "{e}");
            if let Some(s) = e["summary"].as_str() {
                assert!(
                    !s.contains("surprised") && !s.starts_with("Replanning"),
                    "gate closed but a surprise step leaked: {e}"
                );
            }
        }
    }

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

// ── S4: surprise detection + replan-on-surprise ──────────────────────────

/// A one-step plan whose declared predicate is guaranteed to FAIL against
/// the real `time_now` result (a timestamp has no `/entries` array), with
/// the given `on_surprise` action and tool args.
fn failing_plan(id: &str, args: Value, on_surprise: &str) -> Value {
    json!({
        "goal": "list the entries",
        "steps": [{
            "id": id,
            "intent": format!("probe via {id}"),
            "tool": "time.now",
            "args_template": args,
            "depends_on": [],
            "expected": {
                "predicates": [{"kind": "count_at_least", "path": "/entries", "n": 1}],
                "assertion": "an entries array with at least one item",
                "on_surprise": on_surprise,
                "confidence": 0.9
            }
        }],
        "reasoning_trace": "probe then read",
        "plan_version": 1
    })
}

/// A synthesis-only revision — the recovery plan the mock reasoner hands
/// back on replan.
fn synthesis_revision() -> Value {
    json!({
        "goal": "answer from what we have",
        "steps": [{
            "id": "r1",
            "intent": "compose the answer from the results so far",
            "tool": null,
            "args_template": {},
            "depends_on": [],
            "expected": {
                "predicates": [],
                "assertion": "",
                "on_surprise": "continue",
                "confidence": 1.0
            }
        }],
        "reasoning_trace": "the probe surprised; answer directly",
        "plan_version": 1
    })
}

fn unary_reply(content: &str, prompt: u64, eval: u64) -> Value {
    json!({
        "message": {"content": content},
        "done_reason": "stop",
        "prompt_eval_count": prompt,
        "eval_count": eval,
    })
}

fn reasoning_step_summaries(events: &[Value]) -> Vec<String> {
    events_of_type(events, "step")
        .iter()
        .filter(|e| e["stage"] == "reasoning")
        .map(|e| e["summary"].as_str().unwrap().to_owned())
        .collect()
}

/// The core S4 flow: a planted L1 failure (declared predicate vs the real
/// tool result) trips the surprise, the reasoner is re-consulted with the
/// full surprise context, and the REVISED plan guides the rest of the
/// turn. Cost honesty: both reasoner calls fold into the final meter.
#[tokio::test]
async fn planted_failure_triggers_replan_and_revision_guides() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    *h.unary_script.lock().unwrap() = vec![
        unary_reply(
            &failing_plan("s1", json!({}), "replan").to_string(),
            100,
            40,
        ),
        unary_reply(&synthesis_revision().to_string(), 80, 30),
    ];
    set_stream_script(
        h,
        &[
            "{\"name\": \"time.now\", \"arguments\": {}}",
            "recovered answer",
        ],
    );

    let events = run_streaming_turn(json!({
        "user_message": "list the entries",
        "conversation_id": "rsn-s4-replan-1",
        "model": "stub",
        "depth": "think",
    }))
    .await;

    // PLAN + REPLAN — exactly two reasoner calls.
    assert_eq!(h.unary_count.load(Ordering::SeqCst), 2, "plan + one replan");

    // The replan call carries the full surprise context and the same
    // grammar constraint as PLAN.
    let replan_body = h.unary_bodies.lock().unwrap()[1].clone();
    assert_eq!(
        replan_body["format"]["required"][0], "goal",
        "REPLAN rides the PlanDag schema too"
    );
    let user = replan_body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(user.starts_with("### Goal\nlist the entries"), "{user}");
    assert!(
        user.contains("### Plan under revision (version 1)"),
        "{user}"
    );
    assert!(user.contains("### Executed step results"), "{user}");
    assert!(user.contains("s1 (time_now) →"), "{user}");
    assert!(user.contains("### Surprise"), "{user}");
    assert!(user.contains("/entries has >= 1 item(s)"), "{user}");
    assert!(user.contains("REVISED complete plan JSON"), "{user}");

    // Visible: the surprise step, the Replanning phase, the revised
    // checklist and the revision summary.
    let phases: Vec<&str> = events_of_type(&events, "phase")
        .iter()
        .filter_map(|e| e["phase"].as_str())
        .collect();
    assert!(phases.contains(&"replanning"), "phases: {phases:?}");
    let steps = reasoning_step_summaries(&events);
    assert!(
        steps
            .iter()
            .any(|s| s == "s1 surprised: 1 expected check(s) failed"),
        "surprise step: {steps:?}"
    );
    assert!(
        steps.iter().any(|s| s.starts_with("Replanning (1 of 2)")),
        "replanning step: {steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|s| s.starts_with("r1 · compose the answer")),
        "revised checklist: {steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|s| s.starts_with("Plan revised (v2): 1 step(s)")),
        "revision summary: {steps:?}"
    );

    // The revision guided the next round (synthesis guidance on the tail).
    let round2 = h.stream_bodies.lock().unwrap()[1].clone();
    let msgs = round2["messages"].as_array().unwrap().clone();
    let guidance = msgs.last().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(
        guidance.contains("[plan step 1/1 — r1]")
            && guidance.contains("Synthesis step — no tool call"),
        "revision guidance: {guidance}"
    );

    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "recovered answer"
    );

    // Honest meter: PLAN (100/40) + REPLAN (80/30) + two rounds (7/3 each).
    let usage = events_of_type(&events, "usage");
    let final_usage = usage
        .iter()
        .find(|e| e["done"] == true)
        .expect("final usage");
    assert_eq!(final_usage["prompt_tokens"], 100 + 80 + 7 + 7);
    assert_eq!(final_usage["completion_tokens"], 40 + 30 + 3 + 3);
}

/// L2 gating: an assertion-only step (no predicates, low confidence) gets
/// exactly ONE fast-model check — grammar-constrained, deliberation off,
/// tight output allowance — and a satisfied verdict changes nothing.
#[tokio::test]
async fn l2_fires_only_when_pure_verdict_inconclusive() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    let assertion_only_plan = json!({
        "goal": "tell the time",
        "steps": [{
            "id": "s1",
            "intent": "get the current time",
            "tool": "time.now",
            "args_template": {},
            "depends_on": [],
            "expected": {
                "predicates": [],
                "assertion": "a timestamp is returned",
                "on_surprise": "replan",
                "confidence": 0.5
            }
        }],
        "reasoning_trace": "just ask the clock",
        "plan_version": 1
    });
    *h.unary_script.lock().unwrap() = vec![
        unary_reply(&assertion_only_plan.to_string(), 100, 40),
        unary_reply("{\"satisfied\": true, \"reason\": \"looks right\"}", 20, 10),
    ];
    set_stream_script(h, &["{\"name\": \"time.now\", \"arguments\": {}}", "fine"]);

    let events = run_streaming_turn(json!({
        "user_message": "what time is it?",
        "conversation_id": "rsn-s4-l2-ok",
        "model": "stub",
        "depth": "think",
    }))
    .await;

    // PLAN + the L2 check — and nothing else.
    assert_eq!(h.unary_count.load(Ordering::SeqCst), 2, "plan + one L2");
    let l2_body = h.unary_bodies.lock().unwrap()[1].clone();
    assert_eq!(
        l2_body["think"],
        json!(false),
        "L2 never deliberates (the think-budget lesson)"
    );
    assert_eq!(
        l2_body["options"]["num_predict"],
        json!(256),
        "L2 runs on a tight output allowance"
    );
    assert_eq!(
        l2_body["format"]["required"],
        json!(["satisfied", "reason"]),
        "L2 is grammar-constrained — it never freehands"
    );
    let user = l2_body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(user.contains("Expected: a timestamp is returned"), "{user}");

    // Satisfied ⇒ no surprise, no replan.
    let phases: Vec<&str> = events_of_type(&events, "phase")
        .iter()
        .filter_map(|e| e["phase"].as_str())
        .collect();
    assert!(!phases.contains(&"replanning"), "phases: {phases:?}");
    let steps = reasoning_step_summaries(&events);
    assert!(
        !steps.iter().any(|s| s.contains("surprised")),
        "satisfied L2 must not surprise: {steps:?}"
    );
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "fine"
    );

    // Honest meter: PLAN (100/40) + L2 (20/10) + two rounds (7/3 each).
    let usage = events_of_type(&events, "usage");
    let final_usage = usage
        .iter()
        .find(|e| e["done"] == true)
        .expect("final usage");
    assert_eq!(final_usage["prompt_tokens"], 100 + 20 + 7 + 7);
    assert_eq!(final_usage["completion_tokens"], 40 + 10 + 3 + 3);
}

/// An unsatisfied L2 verdict IS a surprise: the checker's reason feeds the
/// replan prompt and the revision takes over.
#[tokio::test]
async fn l2_dissatisfied_triggers_replan() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    let assertion_only_plan = json!({
        "goal": "tell the time",
        "steps": [{
            "id": "s1",
            "intent": "get the current time",
            "tool": "time.now",
            "args_template": {},
            "depends_on": [],
            "expected": {
                "predicates": [],
                "assertion": "a full ISO timestamp with timezone",
                "on_surprise": "replan",
                "confidence": 0.5
            }
        }],
        "reasoning_trace": "ask the clock",
        "plan_version": 1
    });
    *h.unary_script.lock().unwrap() = vec![
        unary_reply(&assertion_only_plan.to_string(), 100, 40),
        unary_reply(
            "{\"satisfied\": false, \"reason\": \"no timezone in the result\"}",
            20,
            10,
        ),
        unary_reply(&synthesis_revision().to_string(), 80, 30),
    ];
    set_stream_script(
        h,
        &["{\"name\": \"time.now\", \"arguments\": {}}", "recovered"],
    );

    let events = run_streaming_turn(json!({
        "user_message": "what time is it?",
        "conversation_id": "rsn-s4-l2-bad",
        "model": "stub",
        "depth": "think",
    }))
    .await;

    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        3,
        "plan + L2 + replan"
    );
    let steps = reasoning_step_summaries(&events);
    assert!(
        steps
            .iter()
            .any(|s| s == "s1 surprised: outcome check said no"),
        "L2 surprise step: {steps:?}"
    );
    // The checker's reason reaches the replan prompt.
    let replan_body = h.unary_bodies.lock().unwrap()[2].clone();
    let user = replan_body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(user.contains("no timezone in the result"), "{user}");
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "recovered"
    );
}

/// The loop guard: replans are budget-capped (default 2). A plan that
/// keeps surprising exhausts the budget and degrades VISIBLY to plain
/// ReAct — the turn still completes, no ping-pong.
#[tokio::test]
async fn replan_budget_exhaustion_degrades_visibly_to_plain_react() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    *h.unary_script.lock().unwrap() = vec![
        unary_reply(
            &failing_plan("s1", json!({}), "replan").to_string(),
            100,
            40,
        ),
        unary_reply(
            &failing_plan("r1", json!({"probe": 1}), "replan").to_string(),
            80,
            30,
        ),
        unary_reply(
            &failing_plan("r2", json!({"probe": 2}), "replan").to_string(),
            80,
            30,
        ),
    ];
    set_stream_script(
        h,
        &[
            "{\"name\": \"time.now\", \"arguments\": {}}",
            "{\"name\": \"time.now\", \"arguments\": {\"probe\": 1}}",
            "{\"name\": \"time.now\", \"arguments\": {\"probe\": 2}}",
            "best effort answer",
        ],
    );

    let events = run_streaming_turn(json!({
        "user_message": "list the entries",
        "conversation_id": "rsn-s4-budget-1",
        "model": "stub",
        "depth": "think",
    }))
    .await;

    // Initial plan + exactly `replan_budget` (2) replans — the third
    // surprise finds the budget spent and NEVER calls the reasoner again.
    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        3,
        "plan + 2 replans, then the budget gate holds"
    );
    let steps = reasoning_step_summaries(&events);
    assert!(
        steps.iter().any(|s| s.starts_with("Replanning (1 of 2)")),
        "{steps:?}"
    );
    assert!(
        steps.iter().any(|s| s.starts_with("Replanning (2 of 2)")),
        "{steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|s| s == "Replan budget exhausted (2) — continuing without the plan"),
        "visible exhaustion note: {steps:?}"
    );

    // After exhaustion the final round carries NO new guidance (the three
    // guidance messages already in the history stay — append-only tail).
    let round4 = h.stream_bodies.lock().unwrap()[3].clone();
    let guidance_count = round4["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("[plan step"))
        })
        .count();
    assert_eq!(guidance_count, 3, "no fourth guidance after exhaustion");

    // The turn still completes — degrade, never break.
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "best effort answer"
    );
}

/// A planner-declared `abort` action ends the turn CLEANLY on surprise:
/// `turn_aborted` with the dedicated `plan_precondition` reason, never a
/// hang or an error.
#[tokio::test]
async fn abort_action_ends_the_turn_cleanly() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    *h.unary_script.lock().unwrap() = vec![unary_reply(
        &failing_plan("s1", json!({}), "abort").to_string(),
        100,
        40,
    )];
    set_stream_script(h, &["{\"name\": \"time.now\", \"arguments\": {}}"]);

    let events = run_streaming_turn(json!({
        "user_message": "list the entries",
        "conversation_id": "rsn-s4-abort-1",
        "model": "stub",
        "depth": "think",
    }))
    .await;

    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        1,
        "no replan on abort"
    );
    let aborted = events_of_type(&events, "turn_aborted");
    assert_eq!(aborted.len(), 1, "{events:?}");
    assert_eq!(aborted[0]["reason"], "plan_precondition");
    assert!(
        aborted[0]["error"].as_str().unwrap().contains("s1"),
        "{:?}",
        aborted[0]
    );
    assert!(
        events_of_type(&events, "turn_complete").is_empty(),
        "a precondition abort is not a completion"
    );
    // The surprise itself was visible before the abort.
    let steps = reasoning_step_summaries(&events);
    assert!(
        steps
            .iter()
            .any(|s| s == "s1 surprised: 1 expected check(s) failed"),
        "{steps:?}"
    );
}

/// L3 no-progress: a round whose tool calls were ALL duplicate-suppressed
/// (the model re-issuing an already-dispatched call) cannot advance the
/// plan — it trips the replan path instead of ping-ponging to the loop cap.
#[tokio::test]
async fn no_progress_duplicate_round_triggers_replan() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true);
    let two_step_plan = json!({
        "goal": "double-check the time",
        "steps": [
            {
                "id": "s1",
                "intent": "get the time",
                "tool": "time.now",
                "args_template": {},
                "depends_on": [],
                "expected": {
                    "predicates": [{"kind": "non_empty"}],
                    "assertion": "",
                    "on_surprise": "continue",
                    "confidence": 0.9
                }
            },
            {
                "id": "s2",
                "intent": "get the time again",
                "tool": "time.now",
                "args_template": {},
                "depends_on": ["s1"],
                "expected": {
                    "predicates": [{"kind": "non_empty"}],
                    "assertion": "",
                    "on_surprise": "continue",
                    "confidence": 0.9
                }
            }
        ],
        "reasoning_trace": "check twice",
        "plan_version": 1
    });
    *h.unary_script.lock().unwrap() = vec![
        unary_reply(&two_step_plan.to_string(), 100, 40),
        unary_reply(&synthesis_revision().to_string(), 80, 30),
    ];
    set_stream_script(
        h,
        &[
            "{\"name\": \"time.now\", \"arguments\": {}}",
            // Round 2: the model re-issues the SAME call — dedupe
            // suppresses it, the round records nothing.
            "{\"name\": \"time.now\", \"arguments\": {}}",
            "done after revision",
        ],
    );

    let events = run_streaming_turn(json!({
        "user_message": "double-check the time",
        "conversation_id": "rsn-s4-noprog-1",
        "model": "stub",
        "depth": "think",
    }))
    .await;

    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        2,
        "plan + the no-progress replan"
    );
    let steps = reasoning_step_summaries(&events);
    assert!(
        steps
            .iter()
            .any(|s| s == "s2 surprised: no progress (all tool calls were duplicates)"),
        "no-progress surprise: {steps:?}"
    );
    assert!(
        steps.iter().any(|s| s.starts_with("Replanning (1 of 2)")),
        "{steps:?}"
    );
    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "done after revision"
    );
}

// ── S4b: Fast→planning auto-escalation (the narrowed identity contract) ──

/// Persist a full reasoning config (the S4b tests need `auto_escalate`
/// off while `enabled` stays on).
fn set_reasoning(cfg: ReasoningConfig) {
    ReasoningConfig::persist(cfg).expect("persist reasoning config");
}

/// Deterministic HARD tool failures: the deferred voice tools return an
/// `[error] phase_11_deferred: …` envelope on every call — exactly L0's
/// definition, no filesystem or network dependence.
const FAIL_CALL_A: &str = "{\"name\": \"voice.mic.chunks\", \"arguments\": {}}";
const FAIL_CALL_B: &str = "{\"name\": \"voice.wakeword.events\", \"arguments\": {}}";

/// Aaron's narrowed contract, the identity half: reasoning enabled + Fast
/// with ZERO or ONE hard tool failure stays byte-identical to trunk —
/// same event transcript, same request bodies, zero reasoner calls.
#[tokio::test]
async fn narrowed_identity_holds_below_the_escalation_threshold() {
    let (_g, h) = test_guard().await;

    // Script A: no failures at all. Script B: exactly one hard failure.
    for (case, script) in [
        (
            "zero-failure",
            vec!["{\"name\": \"time.now\", \"arguments\": {}}", "done"],
        ),
        ("one-failure", vec![FAIL_CALL_A, "done"]),
    ] {
        let mut transcripts: Vec<Vec<Value>> = Vec::new();
        let mut request_bodies: Vec<Vec<Value>> = Vec::new();
        for enabled in [false, true] {
            set_reasoning_enabled(enabled); // default cfg: auto_escalate ON
            set_stream_script(h, &script);
            h.stream_count.store(0, Ordering::SeqCst);
            h.stream_bodies.lock().unwrap().clear();

            let events = run_streaming_turn(json!({
                "user_message": "poke the tools",
                "conversation_id": format!("rsn-s4b-id-{case}-{enabled}"),
                "model": "stub",
                // no depth field: resolves Fast — the watched tier.
            }))
            .await;
            transcripts.push(normalised(&events));
            request_bodies.push(h.stream_bodies.lock().unwrap().clone());
        }

        assert_eq!(
            h.unary_count.load(Ordering::SeqCst),
            0,
            "{case}: below the threshold the reasoner is never called"
        );
        assert_eq!(
            transcripts[1], transcripts[0],
            "{case}: enabled+Fast below the threshold must stay byte-identical"
        );
        assert_eq!(request_bodies[1].len(), request_bodies[0].len(), "{case}");
        for (b, base) in request_bodies[1].iter().zip(&request_bodies[0]) {
            assert_eq!(
                mask_tool_results(&b["messages"]),
                mask_tool_results(&base["messages"]),
                "{case}: request bodies must match the baseline"
            );
            assert!(b.get("format").is_none(), "{case}: no format key");
        }
        h.unary_count.store(0, Ordering::SeqCst);
    }
}

/// The carve-out: the SECOND hard tool failure escalates the Fast turn to
/// planning — visibly (the escalation step names the failures, the
/// Planning phase fires), at the cheap `think` tier, with the failures as
/// their own grounding block, and the plan guides the rest of the turn.
#[tokio::test]
async fn second_hard_failure_escalates_fast_turn_to_planning() {
    let (_g, h) = test_guard().await;
    set_reasoning_enabled(true); // default: auto_escalate ON, escalate_tier think
    *h.unary_script.lock().unwrap() = vec![unary_reply(&synthesis_revision().to_string(), 100, 40)];
    set_stream_script(h, &[FAIL_CALL_A, FAIL_CALL_B, "planned recovery"]);

    let events = run_streaming_turn(json!({
        "user_message": "poke the tools",
        "conversation_id": "rsn-s4b-escalate-1",
        "model": "stub",
    }))
    .await;

    // Exactly one reasoner call — the escalated PLAN.
    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        1,
        "one escalated PLAN"
    );
    let plan_body = h.unary_bodies.lock().unwrap()[0].clone();
    assert_eq!(
        plan_body["think"],
        json!(false),
        "escalation plans at the think tier (deliberation off)"
    );
    assert_eq!(
        plan_body["options"]["num_predict"],
        json!(2048),
        "think tier: output allowance only"
    );
    assert_eq!(
        plan_body["format"]["required"][0], "goal",
        "escalated PLAN is grammar-constrained like any PLAN"
    );
    let user = plan_body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        user.contains("### Hard tool failures this turn"),
        "the failures ground the escalated plan: {user}"
    );
    assert!(user.contains("voice_mic_chunks"), "{user}");
    assert!(user.contains("voice_wakeword_events"), "{user}");
    assert!(user.contains("phase_11_deferred"), "{user}");

    // Visible WHY + the Planning phase.
    let steps = reasoning_step_summaries(&events);
    assert!(
        steps
            .iter()
            .any(|s| s == "2 hard tool failures — escalating to planning (think)"),
        "escalation step: {steps:?}"
    );
    let phases: Vec<&str> = events_of_type(&events, "phase")
        .iter()
        .filter_map(|e| e["phase"].as_str())
        .collect();
    assert!(phases.contains(&"planning"), "phases: {phases:?}");
    assert!(
        steps
            .iter()
            .any(|s| s.starts_with("r1 · compose the answer")),
        "escalated plan checklist: {steps:?}"
    );

    // The plan guided the next round.
    let round3 = h.stream_bodies.lock().unwrap()[2].clone();
    let msgs = round3["messages"].as_array().unwrap().clone();
    let guidance = msgs.last().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(
        guidance.contains("[plan step 1/1 — r1]"),
        "escalated guidance on the tail: {guidance}"
    );

    assert_eq!(
        events_of_type(&events, "turn_complete")[0]["final_message"],
        "planned recovery"
    );

    // Honest meter: three rounds (7/3 each) + the escalated PLAN (100/40).
    let usage = events_of_type(&events, "usage");
    let final_usage = usage
        .iter()
        .find(|e| e["done"] == true)
        .expect("final usage");
    assert_eq!(final_usage["prompt_tokens"], 100 + 7 + 7 + 7);
    assert_eq!(final_usage["completion_tokens"], 40 + 3 + 3 + 3);
}

/// The knob: `auto_escalate: false` keeps even a two-failure Fast turn
/// byte-identical — no watch, no escalation, zero reasoner calls.
#[tokio::test]
async fn auto_escalate_off_never_escalates() {
    let (_g, h) = test_guard().await;

    let script: &[&str] = &[FAIL_CALL_A, FAIL_CALL_B, "done anyway"];
    let mut transcripts: Vec<Vec<Value>> = Vec::new();
    for enabled in [false, true] {
        set_reasoning(ReasoningConfig {
            enabled,
            auto_escalate: false,
            ..ReasoningConfig::default()
        });
        set_stream_script(h, script);
        h.stream_count.store(0, Ordering::SeqCst);
        h.stream_bodies.lock().unwrap().clear();

        let events = run_streaming_turn(json!({
            "user_message": "poke the tools",
            "conversation_id": format!("rsn-s4b-off-{enabled}"),
            "model": "stub",
        }))
        .await;
        transcripts.push(normalised(&events));
    }

    assert_eq!(
        h.unary_count.load(Ordering::SeqCst),
        0,
        "auto_escalate off ⇒ two hard failures never reach the reasoner"
    );
    assert_eq!(
        transcripts[1], transcripts[0],
        "auto_escalate off ⇒ byte-identical even past the threshold"
    );
    // Restore the shared default config for the rest of the binary.
    set_reasoning_enabled(false);
}
