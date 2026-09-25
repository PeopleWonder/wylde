//! End-to-end `/v1` (and brokered `/api/chat`) over the real named-pipe
//! transport, against a mock `wylde-ollama` + harness (#345).
//!
//! The router unit tests use an in-process fake backend; this binary
//! drives the production [`PipeBackend`] and the brokered `/api/chat`
//! route through real IPC. The mock pipe serves `models.list`,
//! `ollama.show`, `ollama.embed`, `ollama.chat`, `ollama.chat_stream` and
//! `ollama.generate_stream`, and records the payloads it receives, so the
//! test sees exactly what the gateway would send `wylde-ollama`. The flow:
//! models → chat → tool call → stream → embeddings → FIM.
//!
//! ## Why a uniquely named mock pipe
//!
//! The gateway resolves its pipe names from `WYLDE_GATEWAY_OLLAMA_SERVICE` /
//! `WYLDE_GATEWAY_HARNESS_SERVICE`; both are pointed at one mock pipe with
//! a random suffix before any request runs. Binding the production
//! `wylde-ollama` name would make the test fail whenever the real product
//! is running (the action registry is process-wide, so one pipe serves
//! both "services").
//!
//! Windows-only — IPC uses named pipes. The live-product counterpart (the
//! official `openai` client against a running gateway) is the
//! `l3.openai_v1` check in `wylde-release preflight --launch`.

#![cfg(windows)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tower::ServiceExt;
use wylde_gateway::auth::token_cache::{global as token_cache, Device};
use wylde_gateway::middleware::rate_limit::RateLimiter;
use wylde_gateway::routes::openai::aliases::Aliases;
use wylde_gateway::routes::openai::{backend, router_with};
use wylde_shared::ipc;

const TOKEN: &str = "openai-e2e-token";

/// `(action, payload)` of every call the mock received.
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

struct Mock {
    calls: Calls,
    _server: Arc<ipc::PipeServer>,
}

fn record(calls: &Calls, action: &str, payload: &Value) {
    calls
        .lock()
        .unwrap()
        .push((action.to_owned(), payload.clone()));
}

async fn mock() -> &'static Mock {
    static M: OnceCell<Mock> = OnceCell::const_new();
    M.get_or_init(|| async {
        let service = format!("openai-e2e-mock-{}", uuid::Uuid::new_v4().simple());
        std::env::set_var("WYLDE_GATEWAY_OLLAMA_SERVICE", &service);
        std::env::set_var("WYLDE_GATEWAY_HARNESS_SERVICE", &service);
        let calls: Calls = Arc::default();

        let c = calls.clone();
        ipc::register_action("models.list", move |p: Value| {
            record(&c, "models.list", &p);
            async move {
                ipc::Reply::ok(json!({"kind": "all", "count": 2, "models": [
                    {"id": "mock-coder", "kind": "llm", "chat_visible": true},
                    {"id": "mock-embed", "kind": "embed", "chat_visible": false}
                ]}))
            }
        });
        let c = calls.clone();
        ipc::register_action("ollama.show", move |p: Value| {
            record(&c, "ollama.show", &p);
            async move { ipc::Reply::ok(json!({"capabilities": ["completion"]})) }
        });
        let c = calls.clone();
        ipc::register_action("ollama.embed", move |p: Value| {
            record(&c, "ollama.embed", &p);
            async move {
                ipc::Reply::ok(json!({"model": "mock-embed", "embeddings": [[0.1, 0.2, 0.3]], "prompt_eval_count": 3}))
            }
        });
        let c = calls.clone();
        ipc::register_action("ollama.chat", move |p: Value| {
            record(&c, "ollama.chat", &p);
            async move {
                ipc::Reply::ok(json!({"message": {"role": "assistant", "content": "unary"}, "done": true}))
            }
        });
        let c = calls.clone();
        ipc::register_streaming_action("ollama.chat_stream", move |p: Value, tx| {
            record(&c, "ollama.chat_stream", &p);
            async move {
                let frames = if p.get("tools").is_some() {
                    vec![json!({"message": {"role": "assistant", "content": "",
                        "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "src/main.rs"}}}]},
                        "done": false})]
                } else {
                    vec![
                        json!({"message": {"role": "assistant", "content": "Hello"}, "done": false}),
                        json!({"message": {"role": "assistant", "content": " world"}, "done": false}),
                    ]
                };
                for f in frames {
                    let _ = tx.send(Ok(f)).await; // wylde-check: discard-result-ok
                }
                let done = json!({"done": true, "done_reason": "stop", "prompt_eval_count": 7, "eval_count": 2});
                let _ = tx.send(Ok(done)).await; // wylde-check: discard-result-ok
            }
        });
        let c = calls.clone();
        ipc::register_streaming_action("ollama.generate_stream", move |p: Value, tx| {
            record(&c, "ollama.generate_stream", &p);
            async move {
                for f in [
                    json!({"response": "n % 2 == 0", "done": false}),
                    json!({"response": "", "done": true, "done_reason": "stop", "prompt_eval_count": 12, "eval_count": 6}),
                ] {
                    let _ = tx.send(Ok(f)).await; // wylde-check: discard-result-ok
                }
            }
        });

        let server = Arc::new(ipc::PipeServer::new(&service));
        let s = Arc::clone(&server);
        // A dedicated OS thread + runtime so the server outlives every test.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            let _ = rt.block_on(s.accept_loop()); // wylde-check: discard-result-ok
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        token_cache()
            .insert(
                TOKEN.to_owned(),
                Device {
                    device_id: "openai-e2e-device".into(),
                    tier: "read_only".into(),
                },
            )
            .await;
        Mock { calls, _server: server }
    })
    .await
}

/// Serialise the tests: they share the mock's call log.
async fn guard() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    LOCK.lock().await
}

fn v1() -> Router {
    router_with(
        backend::pipe(),
        Aliases::parse("coder=mock-coder,embed=mock-embed"),
        RateLimiter::new(1000),
    )
}

fn last(m: &Mock, action: &str) -> Value {
    let calls = m.calls.lock().unwrap();
    calls
        .iter()
        .rev()
        .find(|(a, _)| a == action)
        .map(|(_, p)| p.clone())
        .expect(action)
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

#[tokio::test]
async fn v1_flow_models_chat_tools_stream_embeddings_fim_over_real_ipc() {
    let _g = guard().await;
    let m = mock().await;
    let app = v1();

    // models
    let (s, b) = call(&app, "GET", "/v1/models", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ids: Vec<String> = json_of(&b)["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["mock-coder", "mock-embed", "coder", "embed"]);

    // chat
    let (s, b) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        json!({"model": "coder", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v = json_of(&b);
    assert_eq!(v["choices"][0]["message"]["content"], "Hello world");
    assert_eq!(v["usage"]["total_tokens"], 9);
    let sent = last(m, "ollama.chat_stream");
    assert_eq!(
        sent["model"], "mock-coder",
        "alias resolved before the pipe"
    );
    assert_eq!(sent["evict_on_cancel"], false);
    assert_eq!(sent["pin_load_options"], true);

    // tool call
    let tools = json!([{"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}]);
    let (s, b) = call(&app, "POST", "/v1/chat/completions",
        json!({"model": "coder", "messages": [{"role": "user", "content": "read main"}], "tools": tools})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v = json_of(&b);
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"src/main.rs\"}"
    );

    // stream
    let (s, b) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        json!({"model": "coder", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b.contains("\"content\":\"Hello\"") && b.contains("\"content\":\" world\""),
        "{b}"
    );
    assert!(b.trim_end().ends_with("data: [DONE]"), "{b}");

    // embeddings
    let (s, b) = call(
        &app,
        "POST",
        "/v1/embeddings",
        json!({"model": "embed", "input": "hello"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(json_of(&b)["data"][0]["embedding"], json!([0.1, 0.2, 0.3]));
    assert_eq!(last(m, "ollama.embed")["model"], "mock-embed");

    // FIM: the mock reports no native insert, and "mock-coder" has no
    // template, so the family must come from a real coder id.
    let (s, b) = call(
        &app,
        "POST",
        "/v1/completions",
        json!({"model": "coder", "prompt": "fn f() {", "suffix": "}"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(json_of(&b)["error"]["code"], "fim_unsupported");
    let (s, b) = call(&app, "POST", "/v1/completions",
        json!({"model": "qwen2.5-coder:3b-base", "prompt": "fn is_even(n: u32) -> bool {\n    ", "suffix": "\n}"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(json_of(&b)["choices"][0]["text"], "n % 2 == 0");
    let sent = last(m, "ollama.generate_stream");
    assert_eq!(sent["raw"], true);
    assert_eq!(sent["fim"], true);
    assert!(sent["prompt"]
        .as_str()
        .unwrap()
        .starts_with("<|fim_prefix|>"));
}

#[tokio::test]
async fn api_chat_is_brokered_through_wylde_ollama_with_the_wylde_sse_format() {
    let _g = guard().await;
    let m = mock().await;
    let app = wylde_gateway::routes::chat::router();

    let (s, b) = call(
        &app,
        "POST",
        "/api/chat",
        json!({"model": "mock-coder", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.starts_with("event: token\n"), "{b}");
    assert!(
        b.contains("\"ok\":true") && b.contains("\"content\":\"Hello\""),
        "{b}"
    );
    assert!(b.contains("event: done\n"), "{b}");
    let sent = last(m, "ollama.chat_stream");
    assert_eq!(sent["stream"], true);
    assert_eq!(
        sent["evict_on_cancel"], false,
        "a disconnect must not unload the model"
    );
    assert!(
        sent.get("pin_load_options").is_none(),
        "internal route keeps caller options"
    );

    let (s, b) = call(
        &app,
        "POST",
        "/api/chat/generate",
        json!({"model": "mock-coder", "prompt": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b.contains("\"response\":\"n % 2 == 0\"") && b.contains("event: done\n"),
        "{b}"
    );

    let (s, b) = call(&app, "POST", "/api/chat",
        json!({"model": "mock-coder", "stream": false, "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b.starts_with("event: done\n") && b.contains("\"content\":\"unary\""),
        "{b}"
    );
}
