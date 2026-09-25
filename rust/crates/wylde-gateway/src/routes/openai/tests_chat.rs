//! Router tests for `POST /v1/chat/completions` over a fake backend.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wylde_shared::ipc::IpcError;

use super::backend::testing::FakeBackend;
use super::tests::{assert_openai_error, registry, send, token};
use super::*;

fn app_with(fake: Arc<FakeBackend>, salvage: &str) -> Router {
    router_from(
        OpenAiState::new(
            fake,
            Aliases::parse("coder=real:1"),
            SalvagePolicy::parse(salvage),
        ),
        RateLimiter::new(1000),
    )
}

fn frames(items: Vec<Value>) -> Vec<Result<Value, IpcError>> {
    items.into_iter().map(Ok).collect()
}

fn done(prompt: u64, completion: u64) -> Value {
    json!({"done": true, "done_reason": "stop", "prompt_eval_count": prompt, "eval_count": completion})
}

fn tools() -> Value {
    json!([{"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}])
}

/// POST a chat request and return `(status, raw body)`.
async fn post_raw(app: &Router, token: &str, body: Value) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The JSON payloads of an SSE body, plus whether it ended with `[DONE]`.
fn sse_events(body: &str) -> (Vec<Value>, bool) {
    let datas: Vec<&str> = body
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .collect();
    let done = datas.last() == Some(&"[DONE]");
    let events = datas
        .iter()
        .filter(|d| **d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    (events, done)
}

#[tokio::test]
async fn non_stream_text_reply_with_usage() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = frames(vec![
        json!({"message": {"role": "assistant", "content": "Hel"}, "done": false}),
        json!({"message": {"role": "assistant", "content": "lo"}, "done": false}),
        done(5, 2),
    ]);
    let fake = Arc::new(fake);
    let app = app_with(fake.clone(), "");
    let t = token().await;
    let body = json!({"model": "coder", "messages": [{"role": "user", "content": "hi"}]});
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "coder");
    assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_eq!(
        v["choices"][0]["message"],
        json!({"role": "assistant", "content": "Hello"})
    );
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        v["usage"],
        json!({"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7})
    );
    let sent = &fake.payloads("chat_stream")[0];
    assert_eq!(sent["model"], "real:1", "alias resolved");
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["evict_on_cancel"], false);
    assert_eq!(sent["pin_load_options"], true);
}

#[tokio::test]
async fn non_stream_structured_tool_call_maps_to_openai() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = frames(vec![json!({
        "message": {"role": "assistant", "content": "",
            "tool_calls": [{"id": "call_x", "function": {"name": "read_file", "arguments": {"path": "a.rs"}}}]},
        "done": true, "done_reason": "stop", "prompt_eval_count": 9, "eval_count": 4
    })]);
    let app = app_with(Arc::new(fake), "");
    let t = token().await;
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "read a.rs"}], "tools": tools()});
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let msg = &v["choices"][0]["message"];
    assert!(
        msg["content"].is_null(),
        "content is null alongside tool calls"
    );
    assert_eq!(
        msg["tool_calls"],
        json!([{"id": "call_x", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}}])
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn non_stream_salvages_a_text_form_tool_call() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = frames(vec![
        json!({"message": {"content": "```json\n{\n  \"name\": \"read_file\",\n  \"arguments\": {\"path\": \"src/main.rs\"}\n}\n```"}, "done": false}),
        done(10, 20),
    ]);
    let app = app_with(Arc::new(fake), "*");
    let t = token().await;
    let body = json!({"model": "qwen2.5-coder:14b", "messages": [{"role": "user", "content": "x"}], "tools": tools()});
    let (_, v, _) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        &body.to_string(),
    )
    .await;
    let msg = &v["choices"][0]["message"];
    assert_eq!(msg["tool_calls"][0]["function"]["name"], "read_file");
    assert_eq!(
        msg["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"src/main.rs\"}"
    );
    assert!(
        msg["content"].is_null(),
        "the salvaged JSON is scrubbed from content"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn salvage_off_leaves_the_text_alone() {
    let mut fake = FakeBackend::new(Ok(registry()));
    let text = "{\"name\": \"read_file\", \"arguments\": {\"path\": \"a\"}}";
    fake.stream_frames = frames(vec![
        json!({"message": {"content": text}, "done": false}),
        done(1, 1),
    ]);
    let app = app_with(Arc::new(fake), "off");
    let t = token().await;
    let body =
        json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": tools()});
    let (_, v, _) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        &body.to_string(),
    )
    .await;
    assert_eq!(v["choices"][0]["message"]["content"], text);
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn stream_emits_content_and_tool_call_deltas_usage_and_done() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = frames(vec![
        json!({"message": {"content": "Reading."}, "done": false}),
        json!({"message": {"content": "", "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "a"}}}]}, "done": false}),
        done(3, 6),
    ]);
    let app = app_with(Arc::new(fake), "off");
    let t = token().await;
    let body = json!({"model": "m", "stream": true, "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": "x"}], "tools": tools()});
    let (status, raw) = post_raw(&app, &t, body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!raw.contains("event:"), "OpenAI frames, not Wylde's");
    let (ev, done) = sse_events(&raw);
    assert!(done, "ends with [DONE]: {raw}");
    assert!(ev.iter().all(|e| e["object"] == "chat.completion.chunk"));
    assert_eq!(
        ev[0]["choices"][0]["delta"],
        json!({"role": "assistant", "content": ""})
    );
    assert_eq!(ev[1]["choices"][0]["delta"], json!({"content": "Reading."}));
    let call = &ev[2]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(call["index"], 0);
    assert_eq!(call["type"], "function");
    assert_eq!(
        call["function"],
        json!({"name": "read_file", "arguments": "{\"path\":\"a\"}"})
    );
    assert_eq!(ev[3]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(ev[4]["choices"], json!([]));
    assert_eq!(ev[4]["usage"]["total_tokens"], 9);
}

#[tokio::test]
async fn stream_with_salvage_holds_text_back_and_emits_the_recovered_call() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = frames(vec![
        json!({"message": {"content": "<tools>\n{\"name\": \"read_file\", "}, "done": false}),
        json!({"message": {"content": "\"arguments\": {\"path\": \"a.rs\"}}\n</tools>"}, "done": false}),
        done(1, 1),
    ]);
    let app = app_with(Arc::new(fake), "*");
    let t = token().await;
    let body = json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "x"}], "tools": tools()});
    let (_, raw) = post_raw(&app, &t, body).await;
    assert!(
        !raw.contains("read_file\\\", "),
        "raw JSON text never streamed: {raw}"
    );
    let (ev, done) = sse_events(&raw);
    assert!(done);
    assert_eq!(
        ev[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    assert_eq!(ev[2]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(ev.len(), 3, "role, tool call, finish: {raw}");
}

#[tokio::test]
async fn stream_mid_stream_error_ends_with_an_error_frame_and_no_done() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = vec![
        Ok(json!({"message": {"content": "partial"}, "done": false})),
        Err(IpcError::new("ollama_unreachable", "connection reset")),
    ];
    let app = app_with(Arc::new(fake), "");
    let t = token().await;
    let body =
        json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "x"}]});
    let (status, raw) = post_raw(&app, &t, body).await;
    assert_eq!(status, StatusCode::OK, "headers were already sent");
    let (ev, done) = sse_events(&raw);
    assert!(!done, "a failed stream must not end with [DONE]");
    assert_eq!(ev.last().unwrap()["error"]["code"], "upstream_unavailable");
}

#[tokio::test]
async fn first_frame_error_is_a_real_http_error_even_when_streaming() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.stream_frames = vec![Err(IpcError::new("model_not_found", "not installed"))];
    let app = app_with(Arc::new(fake), "");
    let t = token().await;
    let body =
        json!({"model": "ghost", "stream": true, "messages": [{"role": "user", "content": "x"}]});
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_openai_error(&v, "model_not_found");
}

#[tokio::test]
async fn bad_request_is_openai_400_and_never_calls_ollama() {
    let fake = Arc::new(FakeBackend::new(Ok(registry())));
    let app = app_with(fake.clone(), "");
    let t = token().await;
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        Some(&t),
        r#"{"model":"m","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_openai_error(&v, "invalid_request_error");
    assert!(fake.payloads("chat_stream").is_empty());
}
