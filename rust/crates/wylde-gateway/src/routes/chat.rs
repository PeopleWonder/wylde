//! `/api/chat` — chat surface: harness-driven turns + Ollama SSE proxies.
//!
//! Rust port of `Gateway/routes/chat.py`. The Python file exposes three
//! POST routes, all ported here:
//!
//! | Python                    | Rust wave | Shape                     |
//! |---------------------------|-----------|---------------------------|
//! | `POST /api/chat`          | 2a.1      | Ollama proxy, NDJSON→SSE  |
//! | `POST /api/chat/generate` | 2a.1      | Ollama proxy, NDJSON→SSE  |
//! | `POST /api/chat/run_turn` | 2a        | harness pipe driver, JSON |
//!
//! ## Ollama SSE proxies — `/api/chat` and `/api/chat/generate`
//!
//! Neither route is pipe-backed: they stream NDJSON straight from the
//! local Ollama daemon at `127.0.0.1:11434` and re-emit it as SSE via
//! [`streaming::ndjson_to_sse`] — the same NDJSON→SSE bridge wave 2c's
//! `POST /api/models/pull` rides. `/api/chat` proxies Ollama's
//! `/api/chat` (multi-turn completion); `/api/chat/generate` proxies
//! Ollama's `/api/generate` (raw single-prompt generation). Both parse
//! the body through [`read_body`] (Python's `_read_body`), default its
//! `stream` field to `true`, and emit `event: token` frames terminated
//! by `event: done` — keyed off the `done` field Ollama sets on its
//! final NDJSON line.
//!
//! Both Ollama SSE proxies gate on `require_local` (loopback +
//! WyldeLink CGNAT allowlist) — the same tier the Python `chat.py`
//! uses. `run_turn` gates on `require_device`, which resolves the
//! caller's Bearer token to a verified device record.
//!
//! ## `chat.run_turn` is the harness pipe driver
//!
//! `POST /api/chat/run_turn` drives a full agent turn via the harness
//! pipe — same shape as the Python handler, same payload, same wire.
//! The verified device's tier is forwarded into the `device_tier` slot
//! so the harness's tool dispatcher can gate destructive calls (the
//! `device_tier` field in the request body is IGNORED — only the tier
//! that device-gate verified is trusted).

use axum::body::Bytes;
use axum::extract::Json;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::post;
use axum::{Extension, Router};
use serde_json::{Map, Value};

use super::common::harness_dispatch;
use crate::auth::{require_device, require_local, Device};
use crate::envelopes::failure;
use crate::middleware::{device_limiter, forward_device_events, per_device_rate_limit};
use crate::proxy_core::HttpMethod;
use crate::streaming::{ndjson_to_sse, DEFAULT_CHUNK_TIMEOUT};

/// Local Ollama daemon URL. Mirrors Python's `chat.py::OLLAMA_URL` (and
/// the identical constant in [`super::models`]). Hard-coded — Ollama
/// always binds localhost in the Wylde launch script.
const OLLAMA_URL: &str = "http://127.0.0.1:11434";

const FORWARDED_OPTIONAL_KEYS: [&str; 5] =
    ["model", "workspace_id", "modality", "turn_id", "timeout"];

/// `POST /api/chat` — SSE-streamed chat completion.
///
/// Proxies Ollama's `/api/chat`. The request body matches Ollama's chat
/// schema; `stream` defaults to `true` when the caller omits it. Each
/// NDJSON line Ollama emits becomes an `event: token` SSE frame; the
/// terminal line (the one carrying `done`) becomes `event: done`.
pub async fn chat(headers: HeaderMap, body: Bytes) -> Response {
    let mut payload = read_body(&headers, &body);
    payload
        .entry("stream".to_owned())
        .or_insert(Value::Bool(true));
    ndjson_to_sse(
        &format!("{OLLAMA_URL}/api/chat"),
        Value::Object(payload),
        HttpMethod::Post,
        "token",
        "done",
        "done",
        DEFAULT_CHUNK_TIMEOUT,
    )
    .await
}

/// `POST /api/chat/generate` — SSE-streamed raw single-prompt generation.
///
/// Proxies Ollama's `/api/generate`. Same body handling and SSE event
/// vocabulary as [`chat`]; only the upstream path differs.
pub async fn generate(headers: HeaderMap, body: Bytes) -> Response {
    let mut payload = read_body(&headers, &body);
    payload
        .entry("stream".to_owned())
        .or_insert(Value::Bool(true));
    ndjson_to_sse(
        &format!("{OLLAMA_URL}/api/generate"),
        Value::Object(payload),
        HttpMethod::Post,
        "token",
        "done",
        "done",
        DEFAULT_CHUNK_TIMEOUT,
    )
    .await
}

/// Parse a request body into a JSON object — Rust port of
/// `chat.py::_read_body`.
///
/// An empty body, a `Content-Length: 0` header, or invalid JSON all
/// collapse to an empty map (Python's bare-`{}` returns). A non-object
/// JSON value is wrapped under `_raw`, mirroring Python's
/// `{"_raw": payload}` fallback. Content-type is deliberately ignored,
/// matching `request.json()`.
fn read_body(headers: &HeaderMap, raw: &Bytes) -> Map<String, Value> {
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        == Some("0")
    {
        return Map::new();
    }
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => m,
        Ok(other) => {
            let mut wrapped = Map::new();
            wrapped.insert("_raw".to_owned(), other);
            wrapped
        }
        Err(_) => Map::new(),
    }
}

/// `POST /api/chat/run_turn` — drive one harness chat turn, tier-gated.
///
/// Mandatory body fields: `user_message`, `conversation_id`. Optional
/// pass-through fields: `model`, `workspace_id`, `modality`, `turn_id`,
/// `timeout`. The verified device's tier overrides any `device_tier`
/// field in the body.
pub async fn run_turn(Extension(device): Extension<Device>, body: Option<Json<Value>>) -> Response {
    let body_map: Map<String, Value> = body
        .and_then(|Json(v)| {
            if let Value::Object(m) = v {
                Some(m)
            } else {
                None
            }
        })
        .unwrap_or_default();

    let user_message = body_map
        .get("user_message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let conversation_id = body_map
        .get("conversation_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if user_message.is_empty() || conversation_id.is_empty() {
        return failure(
            "bad_request",
            "user_message and conversation_id are required",
            StatusCode::BAD_REQUEST,
        );
    }

    let mut payload = Map::new();
    payload.insert("user_message".into(), Value::String(user_message));
    payload.insert("conversation_id".into(), Value::String(conversation_id));
    payload.insert("device_tier".into(), Value::String(device.tier));
    for key in FORWARDED_OPTIONAL_KEYS {
        if let Some(v) = body_map.get(key) {
            if !v.is_null() {
                payload.insert(key.to_owned(), v.clone());
            }
        }
    }

    harness_dispatch("chat.run_turn", Value::Object(payload)).await
}

/// Build the chat sub-router: the two Ollama-proxy SSE routes plus the
/// harness-pipe `run_turn` driver.
pub fn router() -> Router {
    Router::new()
        .route("/api/chat", post(chat).route_layer(from_fn(require_local)))
        .route(
            "/api/chat/generate",
            post(generate).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/chat/run_turn",
            post(run_turn)
                // Both per-route layers mount inner to `require_device`
                // so the verified `Device` extension is already in place.
                // `forward_device_events` drains the device's pending-
                // event queue onto the `X-Wylde-Events` response header;
                // `per_device_rate_limit` caps requests per device so a
                // runaway device can't starve its WyldeLink-CGNAT peers.
                // The rate-limit layer sits outer to events: a 429'd
                // request never reaches `forward_device_events`, so its
                // pending-event queue is left intact.
                .route_layer(from_fn(forward_device_events))
                .route_layer(from_fn_with_state(device_limiter(), per_device_rate_limit))
                .route_layer(from_fn(require_device)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tower::ServiceExt;

    #[tokio::test]
    async fn run_turn_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat/run_turn")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({"user_message": "hi", "conversation_id": "c1"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "missing_token");
    }

    #[tokio::test]
    async fn run_turn_path_not_get() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/run_turn")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // GET on the POST-only route should be method-not-allowed.
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// POST an Ollama-proxy route from a non-local caller; assert the
    /// canonical `403 auth_local_denied` envelope `require_local` emits.
    async fn assert_post_local_denied(uri: &str, body: &str) {
        let app = router();
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "POST {uri} should 403 for a non-local caller"
        );
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn chat_rejects_non_local_caller() {
        assert_post_local_denied("/api/chat", r#"{"model":"llama3"}"#).await;
    }

    #[tokio::test]
    async fn generate_rejects_non_local_caller() {
        assert_post_local_denied("/api/chat/generate", r#"{"model":"llama3","prompt":"hi"}"#).await;
    }

    #[tokio::test]
    async fn chat_get_method_not_allowed() {
        // The Ollama proxies are POST-only. A GET hits the *mounted*
        // route and is rejected as method-not-allowed — a 405 (not a
        // 404) also proves wave 2a.1 actually mounted `/api/chat`.
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/chat")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn read_body_empty_is_empty_map() {
        assert!(read_body(&HeaderMap::new(), &Bytes::new()).is_empty());
    }

    #[test]
    fn read_body_content_length_zero_is_empty_map() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, "0".parse().unwrap());
        assert!(read_body(&h, &Bytes::from_static(br#"{"a":1}"#)).is_empty());
    }

    #[test]
    fn read_body_object_round_trips() {
        let m = read_body(
            &HeaderMap::new(),
            &Bytes::from_static(br#"{"model":"llama3"}"#),
        );
        assert_eq!(m.get("model").and_then(Value::as_str), Some("llama3"));
    }

    #[test]
    fn read_body_non_object_wraps_under_raw() {
        // Python's `_read_body` returns `{"_raw": payload}` for any
        // non-dict JSON value.
        let m = read_body(&HeaderMap::new(), &Bytes::from_static(b"[1,2,3]"));
        assert_eq!(m["_raw"], json!([1, 2, 3]));
    }

    #[test]
    fn read_body_invalid_json_is_empty_map() {
        assert!(read_body(&HeaderMap::new(), &Bytes::from_static(b"{not json")).is_empty());
    }

    #[tokio::test]
    async fn ndjson_chat_stream_emits_token_then_done() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // One-shot mock Ollama: emit two NDJSON lines — the second
        // flagged `done` — then close. Mirrors the proxy_core mock
        // server pattern. This exercises the SSE stream shape with the
        // exact event vocabulary the `/api/chat` handler configures:
        // `event_name="token"`, `done_event`/`done_field="done"`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await; // wylde-check: discard-result-ok
                let ndjson = "{\"message\":{\"content\":\"Hi\"},\"done\":false}\n{\"done\":true}\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    ndjson.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await; // wylde-check: discard-result-ok
                let _ = sock.write_all(ndjson.as_bytes()).await; // wylde-check: discard-result-ok
                let _ = sock.shutdown().await; // wylde-check: discard-result-ok
            }
        });

        let url = format!("http://{addr}/api/chat");
        let resp = ndjson_to_sse(
            &url,
            json!({"model": "llama3", "stream": true}),
            HttpMethod::Post,
            "token",
            "done",
            "done",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            Some("text/event-stream"),
        );
        let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: token\n"), "got: {s:?}");
        assert!(s.contains(r#""content":"Hi""#), "got: {s:?}");
        assert!(s.contains("event: done\n"), "got: {s:?}");
        handle.await.ok(); // wylde-check: discard-result-ok
    }
}
