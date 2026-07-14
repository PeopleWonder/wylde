//! Agentic-reasoning **outcome eval** (plan slice S6, scope §6.2).
//!
//! Every prior reasoning slice measured latency, JSON validity and byte
//! identity. **None measured whether planning/surprise/reflection produce
//! better ANSWERS than the plain ReAct loop.** This is the honest test of
//! that question.
//!
//! ## What it does
//!
//! Drives the REAL streaming turn driver (`handle_start_turn` +
//! `handle_stream_turn`) over a fixed corpus of grounded tasks, once per
//! ARM, against the LIVE default reasoner on Ollama. Arms:
//!
//! * `fast`         — reasoning OFF (plain ReAct). The control.
//! * `think`        — PLAN on, deliberation off (grammar-first).
//! * `think_harder` — PLAN on, 4096 think budget.
//! * `ultrathink`   — PLAN on, 10240 think budget.
//! * `fast_auto`    — reasoning on but Fast tier + auto-escalate (S4b): a
//!                    Fast turn that escalates to planning after 2 hard
//!                    tool failures. (Optional, `--arms` opt-in.)
//!
//! ## How it stays hermetic + re-runnable
//!
//! Only **Ollama** must be up. The eval stands up its own in-process
//! `PipeServer` that registers `ollama.chat` / `ollama.chat_stream` /
//! `ollama.embed` and **proxies them faithfully to Ollama HTTP**
//! (`/api/chat`, `/api/embed`) — dropping the pipe-only `priority` knob
//! and forcing `stream` exactly as `wylde-ollama` does, forwarding
//! `model`/`messages`/`tools`/`format`/`think`/`options`/`keep_alive`
//! verbatim. The workspaces service is pointed at a dead pipe (the turn
//! driver degrades fail-soft), so no daemon, broker or Memgraph is
//! needed. The tools are the **real shipped catalog** (the eight
//! `wylde_*` verbs over `time` / `fs_file` / `fs_dir` / `graph` / …), so
//! this exercises the production tool path, not a mock.
//!
//! Because grounding (concept routing + hierarchy + workspace RAG) needs
//! the workspaces service, it is DEGRADED here — this eval isolates the
//! value of the **planning machinery** (PLAN / surprise-replan /
//! escalate / reflect gap-round), not the value of rich grounding. That
//! is called out honestly in the report.
//!
//! ## Run
//!
//! ```text
//! cargo run --release --example reasoning_eval -- \
//!     --arms fast,think,think_harder,ultrathink --reps 3 --out outputs
//! # quick check:
//! cargo run --release --example reasoning_eval -- --smoke
//! # reflection cross-task experiment:
//! cargo run --release --example reasoning_eval -- --reflect
//! ```
//!
//! Windows-only (IPC is named pipes), like the rest of the turn suite.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wylde_harness::turn::actions as chat;
use wylde_harness::turn::reasoning::{Depth, ModelSlots, ReasoningConfig, ReflectGate};
use wylde_shared::ipc::{self, IpcError};

// ─────────────────────────────────────────────────────────────────────────
// Shared proxy state (turns run strictly sequentially, so one recorder is
// safe). `stream_bodies` = the fast execution-round requests (the tool
// trace lives in their `messages`); `unary_bodies` = the reasoner PLAN /
// REPLAN / L2 / REFLECT calls.
// ─────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Recorder {
    stream_bodies: Mutex<Vec<Value>>,
    unary_bodies: Mutex<Vec<Value>>,
    /// The reasoner's RESPONSES to the unary PLAN/REPLAN/L2/REFLECT calls —
    /// i.e. the actual plan JSON the driver parsed. Diagnostic only.
    unary_responses: Mutex<Vec<Value>>,
    seed: AtomicU64,
}

use std::sync::atomic::AtomicBool;
static DUMP: AtomicBool = AtomicBool::new(false);

fn recorder() -> &'static Recorder {
    static R: OnceLock<Recorder> = OnceLock::new();
    R.get_or_init(Recorder::default)
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("http client")
    })
}

fn ollama_host() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with("http") {
                s
            } else {
                format!("http://{s}")
            }
        })
        .unwrap_or_else(|| "http://127.0.0.1:11434".to_owned())
}

/// Faithful `wylde-ollama` translation: drop the pipe-only `priority`,
/// force `stream`, inject the eval seed for reproducibility, forward the
/// rest verbatim.
fn prepare_forward(mut body: Value, stream: bool) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("priority");
        obj.insert("stream".into(), json!(stream));
        let seed = recorder().seed.load(Ordering::SeqCst);
        if seed != 0 {
            let opts = obj
                .entry("options")
                .or_insert_with(|| json!({}))
                .as_object_mut();
            if let Some(o) = opts {
                o.entry("seed").or_insert(json!(seed));
            }
        }
    }
    body
}

async fn ollama_chat_unary(payload: Value) -> Result<Value, IpcError> {
    recorder()
        .unary_bodies
        .lock()
        .unwrap()
        .push(payload.clone());
    let body = prepare_forward(payload, false);
    let resp = http()
        .post(format!("{}/api/chat", ollama_host()))
        .json(&body)
        .send()
        .await
        .map_err(|e| IpcError::new("ollama_proxy", format!("chat send: {e}")))?;
    let v = resp
        .json::<Value>()
        .await
        .map_err(|e| IpcError::new("ollama_proxy", format!("chat decode: {e}")))?;
    recorder().unary_responses.lock().unwrap().push(v.clone());
    Ok(v)
}

async fn ollama_embed(payload: Value) -> Result<Value, IpcError> {
    let body = prepare_forward(payload, false);
    let resp = http()
        .post(format!("{}/api/embed", ollama_host()))
        .json(&body)
        .send()
        .await
        .map_err(|e| IpcError::new("ollama_proxy", format!("embed send: {e}")))?;
    resp.json::<Value>()
        .await
        .map_err(|e| IpcError::new("ollama_proxy", format!("embed decode: {e}")))
}

/// Register the three proxy actions + start the pipe server. Mirrors the
/// `reasoning_plan_e2e` mock harness, but the handlers hit real Ollama.
fn start_proxy(service: &str) -> Arc<ipc::PipeServer> {
    ipc::register_action("ollama.chat", move |payload: Value| async move {
        match ollama_chat_unary(payload).await {
            Ok(v) => ipc::Reply::ok(v),
            Err(e) => ipc::Reply::err(e),
        }
    });
    ipc::register_action("ollama.embed", move |payload: Value| async move {
        match ollama_embed(payload).await {
            Ok(v) => ipc::Reply::ok(v),
            Err(e) => ipc::Reply::err(e),
        }
    });
    ipc::register_streaming_action(
        "ollama.chat_stream",
        move |payload: Value, sender: ipc::StreamSender| async move {
            recorder()
                .stream_bodies
                .lock()
                .unwrap()
                .push(payload.clone());
            let body = prepare_forward(payload, true);
            let resp = match http()
                .post(format!("{}/api/chat", ollama_host()))
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = sender
                        .send(Err(IpcError::new(
                            "ollama_proxy",
                            format!("stream send: {e}"),
                        )))
                        .await;
                    return;
                }
            };
            // Ollama streams NDJSON. The eval only analyses AFTER the turn
            // completes, so buffering the whole body then forwarding each
            // line frame-by-frame is behaviourally identical to true
            // streaming for the harness consumer — and avoids reqwest's
            // `stream` feature. Each line is one native Ollama frame.
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    let _ = sender
                        .send(Err(IpcError::new("ollama_stream_error", format!("{e}"))))
                        .await;
                    return;
                }
            };
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if v.get("error").is_some() {
                        let _ = sender
                            .send(Err(IpcError::new(
                                "ollama_stream_error",
                                v["error"].to_string(),
                            )))
                            .await;
                        return;
                    }
                    let _ = sender.send(Ok(v)).await;
                }
            }
        },
    );

    let server = Arc::new(ipc::PipeServer::new(service));
    let server_clone = Arc::clone(&server);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("proxy runtime");
        let _ = rt.block_on(server_clone.accept_loop());
    });
    server
}

// ─────────────────────────────────────────────────────────────────────────
// The corpus
// ─────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Category {
    /// Planning should NOT help (one-shot). The *Illusion-of-Thinking*
    /// probe: Deep must not lose on easy tasks.
    Simple,
    /// Multi-step / dependency — planning plausibly helps.
    MultiStep,
    /// A planted hard-failing tool; recovery = route around it.
    Recovery,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::Simple => "simple",
            Category::MultiStep => "multi-step",
            Category::Recovery => "recovery",
        }
    }
}

/// Programmatic success: the final answer must contain at least one of the
/// needle groups (each group is AND over its members, the outer list is OR).
/// Case-insensitive.
struct Task {
    id: &'static str,
    category: Category,
    prompt: String,
    /// OR-of-AND needle groups checked against the final answer.
    needles: Vec<Vec<String>>,
    /// A substring of the tool name expected to HARD-FAIL on a recovery
    /// task (e.g. "graph"); the recovery signal is: this failed AND a
    /// non-failing tool later produced the answer.
    failing_tool: Option<&'static str>,
}

fn n(items: &[&str]) -> Vec<Vec<String>> {
    // Each item is an independent OR alternative.
    items.iter().map(|s| vec![s.to_lowercase()]).collect()
}

fn build_corpus(fx: &Path) -> Vec<Task> {
    let p = |rel: &str| fx.join(rel).display().to_string().replace('\\', "/");
    let readme = p("README.md");
    let config = p("config.json");
    let src = p("src");
    let glossary = p("notes/glossary.md");

    vec![
        Task {
            id: "A1_time",
            category: Category::Simple,
            prompt: "Use the appropriate Wylde tool to read the EXACT current \
                     date and time from the system clock, then report it. Do \
                     not answer from memory — call a tool."
                .into(),
            // The live clock is 2026; a from-memory guess would miss it.
            needles: n(&["2026"]),
            failing_tool: None,
        },
        Task {
            id: "A2_read",
            category: Category::Simple,
            prompt: format!(
                "Read the text file at `{readme}` and tell me the single word \
                 printed on its very first line. Reply with just that word."
            ),
            needles: n(&["wylderoot"]),
            failing_tool: None,
        },
        Task {
            id: "B1_dep_chain",
            category: Category::MultiStep,
            prompt: format!(
                "The JSON file at `{config}` has a field `active_module` naming \
                 a module. Under the directory `{src}` there is a Python file \
                 named `<active_module>.py`. Open it and report the name of the \
                 function it defines (the identifier after `def`)."
            ),
            needles: n(&["compute_invoice_total"]),
            failing_tool: None,
        },
        Task {
            id: "B2_search",
            category: Category::MultiStep,
            prompt: format!(
                "Exactly one file under the directory `{src}` contains the \
                 literal marker text WYLDE_ANCHOR_ZK9. Find which file it is and \
                 report that file's name."
            ),
            needles: n(&["billing.py"]),
            failing_tool: None,
        },
        Task {
            id: "C1_graph_recover",
            category: Category::Recovery,
            prompt: format!(
                "Report the numeric value of the concept `tax_rate`. First try \
                 the knowledge graph. If the graph is unavailable, fall back to \
                 reading the glossary file at `{glossary}`, which defines it. \
                 Report the number."
            ),
            needles: n(&["0.0875", ".0875", "8.75"]),
            failing_tool: Some("graph"),
        },
        Task {
            id: "B3_count",
            category: Category::MultiStep,
            prompt: format!(
                "List the files in the directory `{src}` and report exactly how \
                 many Python (`.py`) files it contains. Answer with the number."
            ),
            needles: n(&["2", "two"]),
            failing_tool: None,
        },
    ]
}

/// The two-task reflection experiment (teach → apply). Kept separate from
/// the arm sweep because it needs a controlled long-term store.
struct ReflectPair {
    teach: Task,
    apply: Task,
}

fn build_reflect_pair(fx: &Path) -> ReflectPair {
    let p = |rel: &str| fx.join(rel).display().to_string().replace('\\', "/");
    let glossary = p("notes/glossary.md");
    let glossary2 = p("notes/shipping.md");
    ReflectPair {
        teach: Task {
            id: "R_teach",
            category: Category::Recovery,
            prompt: format!(
                "Report the numeric value of the concept `tax_rate`. Try the \
                 knowledge graph first; if it is unavailable, read the glossary \
                 at `{glossary}`. Report the number."
            ),
            needles: n(&["0.0875", ".0875", "8.75"]),
            failing_tool: Some("graph"),
        },
        apply: Task {
            id: "R_apply",
            category: Category::Recovery,
            prompt: format!(
                "Report the flat `shipping_fee`. Try the knowledge graph first; \
                 if it is unavailable, read the notes at `{glossary2}`. Report \
                 the dollar amount."
            ),
            needles: n(&["12.50", "12.5", "$12"]),
            failing_tool: Some("graph"),
        },
    }
}

fn write_fixture(root: &Path) -> PathBuf {
    let fx = root.join("fixture");
    let _ = std::fs::remove_dir_all(&fx);
    std::fs::create_dir_all(fx.join("src")).unwrap();
    std::fs::create_dir_all(fx.join("notes")).unwrap();
    std::fs::write(fx.join("README.md"), "WYLDEROOT\nThe fixture project.\n").unwrap();
    std::fs::write(
        fx.join("config.json"),
        "{\n  \"active_module\": \"billing\",\n  \"version\": \"3.2.1\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        fx.join("src/billing.py"),
        "# billing module  WYLDE_ANCHOR_ZK9\nTAX_RATE = 0.0875\n\n\
         def compute_invoice_total(subtotal):\n    return subtotal * (1 + TAX_RATE)\n",
    )
    .unwrap();
    std::fs::write(
        fx.join("src/shipping.py"),
        "def compute_shipping(weight):\n    return 12.50\n",
    )
    .unwrap();
    std::fs::write(
        fx.join("notes/glossary.md"),
        "# Glossary\n\ntax_rate: the fraction 0.0875 applied to invoice subtotals.\n",
    )
    .unwrap();
    std::fs::write(
        fx.join("notes/shipping.md"),
        "# Shipping notes\n\nshipping_fee: a flat $12.50 charged per order.\n",
    )
    .unwrap();
    fx
}

// ─────────────────────────────────────────────────────────────────────────
// Arms
// ─────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Arm {
    #[allow(dead_code)]
    name: &'static str,
    cfg: ReasoningConfig,
    #[allow(dead_code)]
    depth: Option<&'static str>,
}

fn arm(name: &str) -> Arm {
    let base = ReasoningConfig {
        // Isolate the PLANNING machinery: no cross-turn lesson writes in
        // the arm sweep (reflection is measured in its own experiment).
        reflect_gate: ReflectGate::Off,
        slots: ModelSlots::default(),
        ..ReasoningConfig::default()
    };
    match name {
        "fast" => Arm {
            name: "fast",
            cfg: ReasoningConfig {
                enabled: false,
                ..base
            },
            depth: Some("fast"),
        },
        "fast_auto" => Arm {
            name: "fast_auto",
            cfg: ReasoningConfig {
                enabled: true,
                default_depth: Depth::Fast,
                auto_escalate: true,
                ..base
            },
            depth: Some("fast"),
        },
        "think" => Arm {
            name: "think",
            cfg: ReasoningConfig {
                enabled: true,
                ..base
            },
            depth: Some("think"),
        },
        "think_harder" => Arm {
            name: "think_harder",
            cfg: ReasoningConfig {
                enabled: true,
                ..base
            },
            depth: Some("think_harder"),
        },
        "ultrathink" => Arm {
            name: "ultrathink",
            cfg: ReasoningConfig {
                enabled: true,
                ..base
            },
            depth: Some("ultrathink"),
        },
        other => panic!("unknown arm {other}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// One run
// ─────────────────────────────────────────────────────────────────────────

struct RunOutcome {
    ok: bool,
    final_answer: String,
    /// (tool_name, ok) in dispatch order, from the execution-round messages.
    tools: Vec<(String, bool)>,
    reasoner_calls: usize,
    planned: bool,
    replanned: bool,
    escalated: bool,
    reflected: bool,
    prompt_tokens: u64,
    completion_tokens: u64,
    wall_ms: u128,
    aborted: Option<String>,
    routed_around_failure: bool,
}

fn model_tag() -> String {
    std::env::var("WYLDE_EVAL_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            wylde_harness::turn::reasoning::config::DEFAULT_REASONER_MODEL.to_owned()
        })
}

async fn drive_turn(task: &Task, depth: &str, conv: &str, timeout_s: u64) -> RunOutcome {
    recorder().stream_bodies.lock().unwrap().clear();
    recorder().unary_bodies.lock().unwrap().clear();
    recorder().unary_responses.lock().unwrap().clear();

    let mut payload = json!({
        "user_message": task.prompt,
        "conversation_id": conv,
        "model": model_tag(),
    });
    payload["depth"] = json!(depth);

    let start = Instant::now();
    let reply = chat::handle_start_turn(payload).await;
    if !reply.ok {
        return RunOutcome {
            ok: false,
            final_answer: String::new(),
            tools: vec![],
            reasoner_calls: 0,
            planned: false,
            replanned: false,
            escalated: false,
            reflected: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            wall_ms: start.elapsed().as_millis(),
            aborted: Some(format!("start_turn: {:?}", reply.error)),
            routed_around_failure: false,
        };
    }
    let turn_id = reply.data["turn_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Value, IpcError>>(2048);
    let streamed = tokio::time::timeout(
        Duration::from_secs(timeout_s),
        chat::handle_stream_turn(json!({ "turn_id": turn_id }), tx),
    )
    .await;
    let wall_ms = start.elapsed().as_millis();

    let mut events = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        if let Ok(v) = chunk {
            events.push(v);
        }
    }

    let timed_out = streamed.is_err();

    if DUMP.load(Ordering::SeqCst) {
        eprintln!("\n════════ DUMP: {} depth={} ════════", task.id, depth);
        eprintln!("── reasoner responses (the plan JSON the driver parsed) ──");
        for (i, r) in recorder()
            .unary_responses
            .lock()
            .unwrap()
            .iter()
            .enumerate()
        {
            let content = r["message"]["content"].as_str().unwrap_or("");
            let think = r["message"]["thinking"].as_str().unwrap_or("");
            eprintln!(
                "  [call {i}] done_reason={} eval_count={} think_chars={}",
                r["done_reason"],
                r["eval_count"],
                think.len()
            );
            eprintln!("    content: {}", content);
        }
        eprintln!("── full event stream ──");
        for e in &events {
            let t = e["type"].as_str().unwrap_or("?");
            match t {
                "token" | "thinking" | "usage" => {}
                "step" => eprintln!(
                    "  step[{}] {} {}",
                    e["stage"].as_str().unwrap_or(""),
                    e["summary"].as_str().unwrap_or(""),
                    e["detail"].as_str().unwrap_or("")
                ),
                "phase" => eprintln!("  phase: {}", e["phase"].as_str().unwrap_or("")),
                "turn_aborted" => {
                    eprintln!("  ABORTED reason={} error={}", e["reason"], e["error"])
                }
                "turn_complete" => eprintln!(
                    "  COMPLETE final_message={:?}",
                    e["final_message"].as_str().unwrap_or("")
                ),
                other => eprintln!("  {other}: {e}"),
            }
        }
        eprintln!("── tool trace ──  {:?}", extract_tool_trace());
        eprintln!("════════ END DUMP ════════\n");
    }

    analyze(task, events, wall_ms, timed_out)
}

fn analyze(task: &Task, events: Vec<Value>, wall_ms: u128, timed_out: bool) -> RunOutcome {
    let mut final_answer = String::new();
    let mut aborted = None;
    let mut planned = false;
    let mut replanned = false;
    let mut reflected = false;
    let mut escalated = false;
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;

    for e in &events {
        match e["type"].as_str() {
            Some("turn_complete") => {
                final_answer = e["final_message"].as_str().unwrap_or_default().to_owned();
            }
            Some("turn_aborted") => {
                aborted = Some(e["reason"].as_str().unwrap_or("aborted").to_owned());
            }
            Some("phase") => match e["phase"].as_str() {
                Some("planning") => planned = true,
                Some("replanning") => replanned = true,
                Some("reflecting") => reflected = true,
                _ => {}
            },
            Some("usage") if e["done"] == json!(true) => {
                prompt_tokens = e["prompt_tokens"].as_u64().unwrap_or(0);
                completion_tokens = e["completion_tokens"].as_u64().unwrap_or(0);
            }
            Some("step") => {
                if let Some(s) = e["summary"].as_str() {
                    if s.contains("escalating to planning") {
                        escalated = true;
                    }
                }
            }
            _ => {}
        }
    }
    if timed_out && aborted.is_none() {
        aborted = Some("timeout".to_owned());
    }

    // Reconstruct the tool trace from the longest execution-round messages
    // array (it carries every prior assistant tool_call + tool result).
    let tools = extract_tool_trace();

    let reasoner_calls = recorder().unary_bodies.lock().unwrap().len();

    let hay = final_answer.to_lowercase();
    let ok = !final_answer.is_empty()
        && task
            .needles
            .iter()
            .any(|grp| grp.iter().all(|nd| hay.contains(nd)));

    // Recovery = the planted tool hard-failed AND a non-failing tool
    // afterwards produced the answer (ok).
    let routed_around_failure = match task.failing_tool {
        Some(sub) => {
            let failed_at = tools
                .iter()
                .position(|(name, tool_ok)| !tool_ok && name.contains(sub));
            match failed_at {
                Some(idx) => {
                    ok && tools
                        .iter()
                        .skip(idx + 1)
                        .any(|(name, tool_ok)| *tool_ok && !name.contains(sub))
                }
                None => false,
            }
        }
        None => false,
    };

    RunOutcome {
        ok,
        final_answer,
        tools,
        reasoner_calls,
        planned,
        replanned,
        escalated,
        reflected,
        prompt_tokens,
        completion_tokens,
        wall_ms,
        aborted,
        routed_around_failure,
    }
}

/// Every dispatched tool call, in order, from the round with the most
/// messages (the final round holds the full history). A `role:"tool"`
/// message = one completed dispatch; `[error]` / `[tier_blocked]` content
/// = a failure (L0's exact definition).
fn extract_tool_trace() -> Vec<(String, bool)> {
    let bodies = recorder().stream_bodies.lock().unwrap();
    let longest = bodies
        .iter()
        .max_by_key(|b| b["messages"].as_array().map(|a| a.len()).unwrap_or(0));
    let mut out = Vec::new();
    if let Some(b) = longest {
        if let Some(msgs) = b["messages"].as_array() {
            for m in msgs {
                if m["role"].as_str() == Some("tool") {
                    let name = m["name"].as_str().unwrap_or("?").to_owned();
                    let content = m["content"].as_str().unwrap_or("");
                    // A dispatch is a failure if the runner rendered an
                    // `[error]`/`[tier_blocked]` prefix (L0's hard-failure
                    // definition) OR the tool returned a JSON error envelope
                    // (`{"status":"error"…}` / `{"code":"not_found"…}`), which
                    // L0's structural check catches but the string prefix does
                    // not. This keeps the recovery metric honest about soft
                    // service errors like a down knowledge graph.
                    let failed = content.starts_with("[error]")
                        || content.starts_with("[tier_blocked]")
                        || content.contains("\"status\": \"error\"")
                        || content.contains("\"status\":\"error\"")
                        || content.contains("\"status\": \"not_found\"")
                        || content.contains("\"status\":\"not_found\"");
                    out.push((name, !failed));
                }
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────
// Aggregation + reporting
// ─────────────────────────────────────────────────────────────────────────

struct Row {
    arm: String,
    task: String,
    category: Category,
    rep: usize,
    outcome: RunOutcome,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn pct(num: usize, den: usize) -> f64 {
    if den == 0 {
        0.0
    } else {
        100.0 * num as f64 / den as f64
    }
}

fn main() {
    // ── env MUST be set before any harness call (Config freezes on first use).
    let suffix = std::process::id();
    let service = format!("ollama-eval-{suffix}");
    let scratch = std::env::temp_dir().join(format!("wylde-reasoning-eval-{suffix}"));
    std::fs::create_dir_all(&scratch).unwrap();

    std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &service);
    std::env::set_var("WYLDE_DATA_DIR", &scratch);
    std::env::set_var("WYLDE_HARNESS_WORKSPACES_SERVICE", "wylde-ws-dead-eval");
    std::env::set_var("WYLDE_POST_TURN_EXTRACTION", "off");
    std::env::set_var("WYLDE_AUTO_SUMMARY", "off");
    std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", "8192");

    // ── args
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut arms: Vec<String> = vec![
        "fast".into(),
        "think".into(),
        "think_harder".into(),
        "ultrathink".into(),
    ];
    let mut reps = 3usize;
    let mut only_tasks: Option<Vec<String>> = None;
    let mut out_dir = PathBuf::from("outputs");
    let mut timeout_s = 360u64;
    let mut base_seed = 42u64;
    let mut smoke = false;
    let mut do_reflect = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--arms" => {
                i += 1;
                arms = args[i].split(',').map(|s| s.trim().to_owned()).collect();
            }
            "--reps" => {
                i += 1;
                reps = args[i].parse().unwrap_or(3);
            }
            "--tasks" => {
                i += 1;
                only_tasks = Some(args[i].split(',').map(|s| s.trim().to_owned()).collect());
            }
            "--out" => {
                i += 1;
                out_dir = PathBuf::from(&args[i]);
            }
            "--timeout" => {
                i += 1;
                timeout_s = args[i].parse().unwrap_or(360);
            }
            "--seed" => {
                i += 1;
                base_seed = args[i].parse().unwrap_or(42);
            }
            "--smoke" => smoke = true,
            "--reflect" => do_reflect = true,
            "--dump" => DUMP.store(true, Ordering::SeqCst),
            _ => {}
        }
        i += 1;
    }
    if smoke {
        arms = vec!["fast".into(), "think".into()];
        reps = 1;
        only_tasks = Some(vec!["A2_read".into(), "C1_graph_recover".into()]);
    }

    let fx = write_fixture(&scratch);
    let corpus = build_corpus(&fx);
    let _server = start_proxy(&service);

    wylde_harness::tooling::consent::set_bypass_for_tests(true);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    eprintln!(
        "reasoning_eval: model={} arms={:?} reps={} timeout={}s host={}",
        model_tag(),
        arms,
        reps,
        timeout_s,
        ollama_host()
    );
    // Warm the model once so the first real turn isn't a cold-load outlier.
    rt.block_on(async {
        let _ = ollama_chat_unary(json!({
            "model": model_tag(),
            "messages": [{"role":"user","content":"ready?"}],
            "stream": false,
            "options": {"num_predict": 1},
        }))
        .await;
    });

    let mut rows: Vec<Row> = Vec::new();

    if do_reflect {
        run_reflection_experiment(&rt, &fx, &out_dir, timeout_s, base_seed);
        return;
    }

    let selected: Vec<&Task> = corpus
        .iter()
        .filter(|t| {
            only_tasks
                .as_ref()
                .map(|f| f.iter().any(|id| id == t.id))
                .unwrap_or(true)
        })
        .collect();

    let total = arms.len() * selected.len() * reps;
    let mut done = 0usize;
    for rep in 0..reps {
        for task in &selected {
            for arm_name in &arms {
                let a = arm(arm_name);
                ReasoningConfig::persist(a.cfg.clone()).expect("persist cfg");
                // Same seed across arms for a given (task, rep) → fair.
                recorder().seed.store(
                    base_seed + (rep as u64) * 1000 + task_hash(task.id),
                    Ordering::SeqCst,
                );
                let depth = a.depth.unwrap_or("fast");
                let conv = format!("eval-{}-{}-{}", arm_name, task.id, rep);
                let outcome = rt.block_on(drive_turn(task, depth, &conv, timeout_s));
                done += 1;
                eprintln!(
                    "[{done}/{total}] {arm_name:<12} {:<18} rep{rep}  ok={}  {:>6}ms  tools={} rc={} {}{}{}{}  ans={:?}",
                    task.id,
                    outcome.ok,
                    outcome.wall_ms,
                    outcome.tools.len(),
                    outcome.reasoner_calls,
                    if outcome.planned { "P" } else { "-" },
                    if outcome.replanned { "R" } else { "-" },
                    if outcome.escalated { "E" } else { "-" },
                    if outcome.aborted.is_some() { "!" } else { "-" },
                    truncate(&outcome.final_answer, 60),
                );
                rows.push(Row {
                    arm: arm_name.clone(),
                    task: task.id.to_owned(),
                    category: task.category,
                    rep,
                    outcome,
                });
            }
        }
    }

    let (json_path, md_path) = write_reports(&out_dir, &arms, &selected, &rows);
    eprintln!("\nwrote {} and {}", json_path.display(), md_path.display());
}

fn task_hash(id: &str) -> u64 {
    id.bytes()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))
        % 997
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn write_reports(
    out_dir: &Path,
    arms: &[String],
    tasks: &[&Task],
    rows: &[Row],
) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(out_dir).ok();

    // Raw JSON.
    let raw: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "arm": r.arm,
                "task": r.task,
                "category": r.category.label(),
                "rep": r.rep,
                "ok": r.outcome.ok,
                "wall_ms": r.outcome.wall_ms,
                "tools": r.outcome.tools.iter().map(|(n,o)| json!({"name":n,"ok":o})).collect::<Vec<_>>(),
                "tool_count": r.outcome.tools.len(),
                "reasoner_calls": r.outcome.reasoner_calls,
                "planned": r.outcome.planned,
                "replanned": r.outcome.replanned,
                "escalated": r.outcome.escalated,
                "reflected": r.outcome.reflected,
                "prompt_tokens": r.outcome.prompt_tokens,
                "completion_tokens": r.outcome.completion_tokens,
                "routed_around_failure": r.outcome.routed_around_failure,
                "aborted": r.outcome.aborted,
                "answer": r.outcome.final_answer,
            })
        })
        .collect();
    let json_path = out_dir.join("reasoning-eval-results.json");
    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&json!({
            "model": model_tag(),
            "rows": raw,
        }))
        .unwrap(),
    )
    .unwrap();

    // Markdown summary.
    let mut md = String::new();
    md.push_str("# Agentic Reasoning — Outcome Eval Results\n\n");
    md.push_str(&format!("Model: `{}`\n\n", model_tag()));
    md.push_str(&format!(
        "Rows: {} ({} arms × {} tasks × reps). Grounding DEGRADED (hermetic \
         harness, workspaces service off) — this isolates the planning machinery.\n\n",
        rows.len(),
        arms.len(),
        tasks.len()
    ));

    // Per-arm aggregate.
    md.push_str("## Per-arm aggregate (all tasks)\n\n");
    md.push_str("| arm | success | median tools | median wall | median tok(compl) | reasoner calls/turn |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for a in arms {
        let ars: Vec<&Row> = rows.iter().filter(|r| &r.arm == a).collect();
        let succ = ars.iter().filter(|r| r.outcome.ok).count();
        let tools = median(ars.iter().map(|r| r.outcome.tools.len() as f64).collect());
        let wall = median(ars.iter().map(|r| r.outcome.wall_ms as f64).collect());
        let tok = median(
            ars.iter()
                .map(|r| r.outcome.completion_tokens as f64)
                .collect(),
        );
        let rc = median(
            ars.iter()
                .map(|r| r.outcome.reasoner_calls as f64)
                .collect(),
        );
        md.push_str(&format!(
            "| {a} | {}/{} ({:.0}%) | {tools:.0} | {:.0} ms | {tok:.0} | {rc:.0} |\n",
            succ,
            ars.len(),
            pct(succ, ars.len()),
            wall,
        ));
    }

    // Per-category success × arm.
    md.push_str("\n## Success rate by category × arm\n\n");
    md.push_str("| category | ");
    for a in arms {
        md.push_str(&format!("{a} | "));
    }
    md.push('\n');
    md.push_str("|---|");
    for _ in arms {
        md.push_str("---|");
    }
    md.push('\n');
    for cat in [Category::Simple, Category::MultiStep, Category::Recovery] {
        md.push_str(&format!("| {} | ", cat.label()));
        for a in arms {
            let ars: Vec<&Row> = rows
                .iter()
                .filter(|r| &r.arm == a && r.category == cat)
                .collect();
            let succ = ars.iter().filter(|r| r.outcome.ok).count();
            md.push_str(&format!(
                "{}/{} ({:.0}%) | ",
                succ,
                ars.len(),
                pct(succ, ars.len())
            ));
        }
        md.push('\n');
    }

    // Recovery detail.
    md.push_str("\n## Recovery (planted graph failure → routed around)\n\n");
    md.push_str("| arm | success | routed-around | median tools |\n|---|---|---|---|\n");
    for a in arms {
        let ars: Vec<&Row> = rows
            .iter()
            .filter(|r| &r.arm == a && r.category == Category::Recovery)
            .collect();
        let succ = ars.iter().filter(|r| r.outcome.ok).count();
        let routed = ars
            .iter()
            .filter(|r| r.outcome.routed_around_failure)
            .count();
        let tools = median(ars.iter().map(|r| r.outcome.tools.len() as f64).collect());
        md.push_str(&format!(
            "| {a} | {}/{} | {}/{} | {tools:.0} |\n",
            succ,
            ars.len(),
            routed,
            ars.len()
        ));
    }

    // Per-task × arm success grid.
    md.push_str("\n## Per-task success (n reps) × arm\n\n");
    md.push_str("| task | cat | ");
    for a in arms {
        md.push_str(&format!("{a} | "));
    }
    md.push('\n');
    md.push_str("|---|---|");
    for _ in arms {
        md.push_str("---|");
    }
    md.push('\n');
    for t in tasks {
        md.push_str(&format!("| {} | {} | ", t.id, t.category.label()));
        for a in arms {
            let ars: Vec<&Row> = rows
                .iter()
                .filter(|r| &r.arm == a && r.task == t.id)
                .collect();
            let succ = ars.iter().filter(|r| r.outcome.ok).count();
            md.push_str(&format!("{}/{} | ", succ, ars.len()));
        }
        md.push('\n');
    }

    let md_path = out_dir.join("reasoning-eval-results.md");
    std::fs::write(&md_path, md).unwrap();
    (json_path, md_path)
}

// ─────────────────────────────────────────────────────────────────────────
// Reflection experiment: does a lesson learned on the teach task improve
// the related apply task? Compares apply-with-lesson vs apply-clean-store.
// ─────────────────────────────────────────────────────────────────────────

fn run_reflection_experiment(
    rt: &tokio::runtime::Runtime,
    fx: &Path,
    out_dir: &Path,
    timeout_s: u64,
    base_seed: u64,
) {
    let pair = build_reflect_pair(fx);
    // Reflection ON, deliberating enough to critique; think tier is the
    // shipped default planning tier.
    let cfg = ReasoningConfig {
        enabled: true,
        reflect_gate: ReflectGate::Always,
        slots: ModelSlots::default(),
        ..ReasoningConfig::default()
    };
    ReasoningConfig::persist(cfg).expect("persist");
    recorder().seed.store(base_seed, Ordering::SeqCst);

    eprintln!("\n== Reflection experiment ==");

    // Arm 1 — teach then apply in the SAME store (lesson present).
    eprintln!("[with-lesson] teach…");
    let teach = rt.block_on(drive_turn(&pair.teach, "think", "reflect-teach", timeout_s));
    eprintln!(
        "  teach ok={} reflected={} tools={} ans={:?}",
        teach.ok,
        teach.reflected,
        teach.tools.len(),
        truncate(&teach.final_answer, 60)
    );
    // Read back what REFLECT stored (PLAN's actual lesson surface).
    let lessons = read_lessons(&scratch_data());
    eprintln!("  lessons stored: {}", lessons.len());
    for l in &lessons {
        eprintln!("    · {}", truncate(l, 100));
    }
    eprintln!("[with-lesson] apply…");
    let apply_warm = rt.block_on(drive_turn(
        &pair.apply,
        "think",
        "reflect-apply-warm",
        timeout_s,
    ));

    // Arm 2 — apply with a CLEAN store (wipe lessons written by teach).
    wipe_lessons(&scratch_data());
    eprintln!("[clean-store] apply…");
    let apply_cold = rt.block_on(drive_turn(
        &pair.apply,
        "think",
        "reflect-apply-cold",
        timeout_s,
    ));

    let graph_calls = |o: &RunOutcome| o.tools.iter().filter(|(n, _)| n.contains("graph")).count();
    let mut md = String::new();
    md.push_str("# Reflection experiment (teach → apply)\n\n");
    md.push_str(&format!(
        "Model `{}`, think tier, reflect_gate=Always.\n\n",
        model_tag()
    ));
    md.push_str(&format!(
        "Teach task `{}`: ok={}, reflected={}, lessons stored={}.\n\n",
        pair.teach.id,
        teach.ok,
        teach.reflected,
        lessons.len()
    ));
    if !lessons.is_empty() {
        md.push_str("Lesson(s) written:\n\n");
        for l in &lessons {
            md.push_str(&format!("- {l}\n"));
        }
        md.push('\n');
    }
    md.push_str("| apply run | store | ok | wall ms | tools | graph attempts | routed-around |\n");
    md.push_str("|---|---|---|---|---|---|---|\n");
    md.push_str(&format!(
        "| {} | with lesson | {} | {} | {} | {} | {} |\n",
        pair.apply.id,
        apply_warm.ok,
        apply_warm.wall_ms,
        apply_warm.tools.len(),
        graph_calls(&apply_warm),
        apply_warm.routed_around_failure,
    ));
    md.push_str(&format!(
        "| {} | clean | {} | {} | {} | {} | {} |\n",
        pair.apply.id,
        apply_cold.ok,
        apply_cold.wall_ms,
        apply_cold.tools.len(),
        graph_calls(&apply_cold),
        apply_cold.routed_around_failure,
    ));
    md.push_str(
        "\nHypothesis: with the teach-lesson present, the apply PLAN should \
         avoid the dead knowledge-graph tool (fewer graph attempts) and/or \
         reach the answer faster. Read the numbers, not the hope.\n",
    );

    std::fs::create_dir_all(out_dir).ok();
    let p = out_dir.join("reasoning-eval-reflection.md");
    std::fs::write(&p, md).unwrap();
    eprintln!("wrote {}", p.display());
}

fn scratch_data() -> PathBuf {
    PathBuf::from(std::env::var("WYLDE_DATA_DIR").unwrap())
}

/// Read the long-term reflection records' bodies (the lesson sentences PLAN
/// would ground on). The store is `<data_dir>/long_term.json` — either a
/// bare array of records or `{records|entries:[...]}`.
fn read_lessons(data_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(s) = std::fs::read_to_string(data_dir.join("long_term.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return out;
    };
    let records: Vec<Value> = if let Some(a) = v.as_array() {
        a.clone()
    } else {
        v["records"]
            .as_array()
            .or_else(|| v["entries"].as_array())
            .cloned()
            .unwrap_or_default()
    };
    for r in records {
        let tags = r["tags"].as_array().cloned().unwrap_or_default();
        let is_reflection = tags.iter().any(|t| {
            t.as_str()
                .map(|s| s.contains("reflection") || s.contains("lesson"))
                .unwrap_or(false)
        });
        if is_reflection {
            if let Some(body) = r["body"].as_str().or_else(|| r["text"].as_str()) {
                out.push(body.to_owned());
            }
        }
    }
    out
}

fn wipe_lessons(data_dir: &Path) {
    let _ = std::fs::remove_file(data_dir.join("long_term.json"));
    let _ = std::fs::remove_file(data_dir.join("long_term.vec.bin"));
}
