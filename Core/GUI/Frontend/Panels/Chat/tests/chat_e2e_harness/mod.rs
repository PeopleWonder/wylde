//! Fixture for the all-surfaces chat-turn e2e (`../chat_turn_e2e.rs`, #236).
//!
//! Split out of that file to keep both under the 700-line cap; the *why* of
//! every choice here — what is real, what is stubbed, and why the transport
//! seam sits where it does — is documented at the top of `chat_turn_e2e.rs`.
//! Read that first.
//!
//! A directory module on purpose: `tests/foo.rs` would be compiled as its own
//! test binary, `tests/foo/mod.rs` is not.

#![allow(dead_code)] // helpers are used from the parent test binary

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use gpui::TestAppContext;
use wylde_gpui_input::InputEvent;
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope};

// ─────────────────────────────────────────────────────────────────────────
// Fixture — one real harness pipe + one stub inference pipe per binary
// ─────────────────────────────────────────────────────────────────────────

/// What the stub inference backend answers with. Distinctive on purpose: if
/// this string reaches the bubble, it came through the driver and the stream.
pub const STUB_REPLY: &str = "stub-reply::the turn traversed the real driver";

/// Upper bound on one end-to-end turn. Generous — the stub answers instantly,
/// so this only ever trips on a genuinely stuck path, never on slowness.
pub const TURN_TIMEOUT: Duration = Duration::from_secs(30);

/// The GUI transport, wired to the **real** harness action registry.
///
/// This plugs into `wylde_gui_pipe`'s single call chokepoint and answers by
/// dispatching into [`wylde_shared::ipc`] — exactly what the pipe server does
/// on the other side of a socket. It scripts nothing: every reply is whatever
/// the production handler produced.
///
/// Synchronous by design. `FakeBackend`'s methods return values, not futures,
/// so the whole harness round-trip runs inside the gpui task's own poll on the
/// test thread. Nothing ever wakes a gpui task from another thread, which is
/// what keeps the test deterministic under gpui's test scheduler.
pub struct HarnessBackend {
    rt: &'static tokio::runtime::Runtime,
}

impl wylde_gui_test_support::FakeBackend for HarnessBackend {
    fn call(
        &self,
        _service: &str,
        _http_verb: &str,
        _path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        // `body` is the GUI's own `{"action": …, "payload": …}` envelope —
        // handed through untouched, so a malformed one fails here just as it
        // would on the wire.
        let envelope = body.cloned().unwrap_or(Value::Null);
        let reply = self
            .rt
            .block_on(wylde_shared::ipc::dispatch_action(envelope));
        if reply.ok {
            Ok(reply.data)
        } else {
            let err = reply.error.unwrap_or_else(|| {
                wylde_shared::ipc::IpcError::new("unknown", "handler failed with no error")
            });
            Err(format!("{}: {}", err.code, err.message))
        }
    }

    fn stream(
        &self,
        _service: &str,
        action: &str,
        payload: &Value,
    ) -> Result<Vec<Result<Value, String>>, String> {
        let payload = payload.clone();
        let action = action.to_owned();
        self.rt.block_on(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);
            let fut = wylde_shared::ipc::take_streaming_action(&action, payload, tx)
                .map_err(|e| format!("{}: {}", e.code, e.message))?;
            // Drain concurrently with the handler: `chat.stream_turn` emits
            // more events than any sane channel bound, so collecting after
            // the fact would deadlock on a full channel.
            let driving = tokio::spawn(fut);
            let mut chunks = Vec::new();
            while let Some(chunk) = rx.recv().await {
                chunks.push(chunk.map_err(|e| format!("{}: {}", e.code, e.message)));
            }
            let _ = driving.await;
            Ok(chunks)
        })
    }
}

pub struct Fixture {
    /// Every `ollama.chat_stream` request body the turn driver sent.
    pub inference_requests: Arc<Mutex<Vec<Value>>>,
    /// Every `(verb, payload)` the turn driver sent the workspaces service —
    /// the scoping witness.
    pub workspace_calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// Runtime the harness round-trips are driven on.
    rt: &'static tokio::runtime::Runtime,
    /// Keep the fixture servers alive for the whole binary.
    _ollama_server: Arc<wylde_shared::ipc::PipeServer>,
    _workspaces_server: Arc<wylde_shared::ipc::PipeServer>,
}

/// Spawn `server`'s accept loop on its own thread + runtime (the pattern
/// `reasoning_plan_e2e.rs` uses — each fixture server owns its reactor so the
/// gpui test thread never has to pump it).
pub fn serve(server: &Arc<wylde_shared::ipc::PipeServer>) {
    let server = Arc::clone(server);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture server runtime");
        let _ = rt.block_on(server.accept_loop());
    });
}

pub fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let pid = std::process::id();

        // ── Hermetic environment, set BEFORE the harness reads any config.
        // `Config` freezes service names in a process-wide OnceLock on first
        // use, so this has to happen before the first harness call.
        let data_dir = std::env::temp_dir().join(format!("wylde-chat-e2e-{pid}"));
        std::env::set_var("WYLDE_DATA_DIR", &data_dir);
        // Keep the background post-turn LLM pass off the stub's counters —
        // it would issue extra inference calls after the turn settles.
        std::env::set_var("WYLDE_POST_TURN_EXTRACTION", "off");

        // The turn driver requires a model name (it refuses to guess). The
        // stub answers any name, so this only has to be *set* — it is the
        // string that rides the request, not a model that exists.
        std::env::set_var("WYLDE_DEFAULT_MODEL", "stub-model");

        let ollama_service = format!("ollama-chat-e2e-{pid}");
        std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &ollama_service);
        let workspaces_service = format!("workspaces-chat-e2e-{pid}");
        std::env::set_var("WYLDE_HARNESS_WORKSPACES_SERVICE", &workspaces_service);

        // Tool dispatch is gated on consent; the gate's own semantics are
        // tested elsewhere and a prompt would stall this turn.
        wylde_harness::tooling::consent::set_bypass_for_tests(true);

        // ── The stub inference backend (the ONLY faked hop) ──────────────
        let inference_requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = Arc::clone(&inference_requests);
            wylde_shared::ipc::register_streaming_action(
                "ollama.chat_stream",
                move |payload: Value, sender| {
                    seen.lock().unwrap().push(payload);
                    async move {
                        let _ = sender
                            .send(Ok(json!({
                                "message": { "content": STUB_REPLY },
                                "done": false,
                            })))
                            .await;
                        let _ = sender
                            .send(Ok(json!({
                                "done": true,
                                "prompt_eval_count": 11,
                                "eval_count": 7,
                            })))
                            .await;
                    }
                },
            );
        }
        // The unary surface (the reasoner's PLAN call). A Fast turn — what the
        // InferenceBar sends by default — never reaches it; registered so a
        // config drift produces a deterministic answer, not a dead pipe.
        wylde_shared::ipc::register_action("ollama.chat", move |_p: Value| async move {
            wylde_shared::ipc::Reply::ok(json!({
                "message": { "content": STUB_REPLY },
                "prompt_eval_count": 11,
                "eval_count": 7,
            }))
        });
        // Model-metadata reads the turn's option resolution may make. Answered
        // so they resolve deterministically instead of degrading on a timeout.
        wylde_shared::ipc::register_action("ollama.list_models", |_p: Value| async move {
            wylde_shared::ipc::Reply::ok(json!({ "models": [{ "name": "stub-model" }] }))
        });
        wylde_shared::ipc::register_action("ollama.show", |_p: Value| async move {
            wylde_shared::ipc::Reply::ok(json!({ "model_info": {} }))
        });
        wylde_shared::ipc::register_action("ollama.get_model_defaults", |_p: Value| async move {
            wylde_shared::ipc::Reply::ok(json!({}))
        });
        wylde_shared::ipc::register_action("ollama.embed", |_p: Value| async move {
            wylde_shared::ipc::Reply::ok(json!({ "embeddings": [] }))
        });

        // ── The stub workspaces service — the SCOPING WITNESS ────────────
        // It answers emptily; what it is for is *recording whether it was
        // consulted at all*. A bound (Docked) turn must reach it; an unbound
        // (Global) turn structurally cannot.
        let workspace_calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        for verb in [
            "workspaces.gather_prompt",
            "workspaces.ignore.list",
            "workspaces.anchors.find_by_token",
            "workspaces.symbols.find",
            "workspaces.symbol_context",
        ] {
            let seen = Arc::clone(&workspace_calls);
            wylde_shared::ipc::register_action(verb, move |p: Value| {
                seen.lock().unwrap().push((verb.to_owned(), p));
                async move { wylde_shared::ipc::Reply::ok(json!({})) }
            });
        }

        // ── The REAL harness verb surface ────────────────────────────────
        // `install()` is what the shipped harness binary calls: every verb,
        // registered against the real `DefaultHarnessApi`. `chat.start_turn`
        // and `chat.stream_turn` therefore run the production turn driver.
        wylde_harness::install();

        let ollama_server = Arc::new(wylde_shared::ipc::PipeServer::new(&ollama_service));
        let workspaces_server = Arc::new(wylde_shared::ipc::PipeServer::new(&workspaces_service));
        serve(&ollama_server);
        serve(&workspaces_server);

        // The runtime the harness round-trips are driven on. Multi-threaded:
        // the driver's own calls out to the fixture services need worker
        // threads while the test thread sits in `block_on`. Leaked because it
        // must outlive every case in the binary.
        let rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("harness dispatch runtime"),
        ));

        // Give the two accept loops a moment to bind before the first turn.
        std::thread::sleep(Duration::from_millis(300));

        Fixture {
            inference_requests,
            workspace_calls,
            rt,
            _ollama_server: ollama_server,
            _workspaces_server: workspaces_server,
        }
    })
}

/// RAII guard for one case: holds the serialisation lock and clears the
/// installed backend on drop, so a later test in this binary can never
/// inherit this one's transport.
pub struct CaseGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for CaseGuard {
    fn drop(&mut self) {
        wylde_gui_test_support::clear();
    }
}

/// Serialise the cases, clear the recordings, and install the real-harness
/// transport for this case.
///
/// Serialisation is required, not tidiness: the recordings are process-global
/// (one fixture per binary), and the scoping assertions read "did the
/// workspaces service get called *during this turn*".
pub fn case_guard() -> (CaseGuard, &'static Fixture) {
    static LOCK: Mutex<()> = Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = fixture();
    f.inference_requests.lock().unwrap().clear();
    f.workspace_calls.lock().unwrap().clear();
    wylde_gui_test_support::install(Arc::new(HarnessBackend { rt: f.rt }));
    (CaseGuard { _lock: lock }, f)
}

// ─────────────────────────────────────────────────────────────────────────
// Driving one surface
// ─────────────────────────────────────────────────────────────────────────

/// Mount the `ChatPanel` entity that backs `surface`, scoped the way the real
/// surface is scoped when a user is looking at it.
pub fn mount(
    cx: &mut TestAppContext,
    scope: ChatScope,
    workspace: Option<&str>,
) -> gpui::WindowHandle<ChatPanel> {
    let window = cx.add_window(|_w, cx| ChatPanel::new(scope, cx));
    cx.run_until_parked();
    if let Some(ws) = workspace {
        // Entering a workspace in the Workspaces IDE is what binds the dock —
        // the same call the panel makes off the workspace-scope bus.
        window
            .update(cx, |panel, _w, cx| {
                panel.apply_workspace_scope(Some(ws.to_owned()), cx);
            })
            .unwrap();
        pump(cx, Duration::from_millis(200));
    }
    window
}

/// Type `text` into the surface's composer and press Enter — the real gesture.
/// This is the same seam `type_and_send.rs` enters at: set the text on
/// `prompt_input` and emit `Submit`, exactly as `SubmitMode::EnterSubmits`
/// does on a real keypress, then let the panel's own subscription route it.
pub fn type_and_press_enter(
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

/// Run the gpui executor for `dur` of *real* time.
///
/// `run_until_parked` alone is not enough here and the reason is the whole
/// point of this file: the awaited work is real named-pipe IO on the tokio
/// bridge, not a gpui task. gpui parks with the task pending, the tokio side
/// completes, its waker re-arms the gpui task, and only the *next* pump sees
/// it. So we alternate: drain gpui, yield real time, repeat.
pub fn pump(cx: &mut TestAppContext, dur: Duration) {
    let until = Instant::now() + dur;
    loop {
        cx.run_until_parked();
        if Instant::now() >= until {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Pump until `settled` holds, or fail with `what` after [`TURN_TIMEOUT`].
pub fn pump_until(
    cx: &mut TestAppContext,
    window: &gpui::WindowHandle<ChatPanel>,
    what: &str,
    settled: impl Fn(&ChatPanel) -> bool,
) {
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        cx.run_until_parked();
        let done = window.update(cx, |panel, _w, _cx| settled(panel)).unwrap();
        if done {
            return;
        }
        if Instant::now() >= deadline {
            let diagnosis = window
                .update(cx, |panel, _w, _cx| {
                    format!(
                        "error={:?} active_turn_id={:?} starting={} messages={:?}",
                        panel.error,
                        panel.active_turn_id,
                        panel.starting,
                        panel
                            .messages
                            .iter()
                            .map(|m| (m.content.clone(), m.streaming))
                            .collect::<Vec<_>>()
                    )
                })
                .unwrap();
            panic!("timed out waiting for {what} after {TURN_TIMEOUT:?} — {diagnosis}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The settled assistant bubble's text — what the user actually reads.
pub fn rendered_reply(cx: &mut TestAppContext, window: &gpui::WindowHandle<ChatPanel>) -> String {
    window
        .update(cx, |panel, _w, _cx| {
            panel
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default()
        })
        .unwrap()
}

/// The conversation this surface is currently threaded on — after a turn this
/// is the id the harness echoed back on `chat.start_turn`.
pub fn conversation_id(cx: &mut TestAppContext, window: &gpui::WindowHandle<ChatPanel>) -> String {
    window
        .update(cx, |panel, _w, _cx| panel.conversation_id.clone())
        .unwrap()
}

/// Drive one full turn on `window` and return the rendered assistant text.
pub fn run_turn(
    cx: &mut TestAppContext,
    window: &gpui::WindowHandle<ChatPanel>,
    text: &str,
    what: &str,
) -> String {
    type_and_press_enter(cx, window, text);
    // Fail fast on the "composer isn't wired to the turn path at all" break.
    // `send_user_message` pushes the user + assistant bubbles synchronously, so
    // an empty log here means Enter went nowhere — diagnose it now rather than
    // burning TURN_TIMEOUT waiting for a turn that was never started.
    let started = window
        .update(cx, |panel, _w, _cx| !panel.messages.is_empty())
        .unwrap();
    assert!(
        started,
        "{what}: pressing Enter started no turn — the composer's Submit event \
         never reached `submit_text`, so this surface has no working send"
    );
    // Settled == the panel released the turn AND the bubble stopped streaming.
    // Both, so a half-applied stream can't read as success.
    pump_until(cx, window, what, |panel| {
        panel.active_turn_id.is_none()
            && !panel.starting
            && panel.messages.last().is_some_and(|m| !m.streaming)
    });
    rendered_reply(cx, window)
}
// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

/// Every `messages[].content` string in an `ollama.chat_stream` request body.
pub fn message_contents(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .filter_map(|m| m["content"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
