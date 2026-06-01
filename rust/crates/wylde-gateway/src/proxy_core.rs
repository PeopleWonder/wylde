//! Sibling-service proxy helpers — pipe IPC + localhost HTTP behind one envelope.
//!
//! Rust port of `Gateway/proxy_core.py`. Wave 1 brought the pipe
//! transport online ([`pipe_action`], [`validate_token`]); wave 2c
//! adds the HTTP transport ([`http_call`]) used by the routes that
//! talk to local sibling daemons (Ollama, n8n).
//!
//! ## Why a sibling helper rather than the egress allowlist
//!
//! Per Wylde principle #11 (network boundaries), localhost daemons —
//! Ollama, n8n, the WyldeLink management API, the live Wylde pipes —
//! are NOT on the egress path. They never leave the machine, so the
//! allowlist + kill switch + audit log don't police them. Routes that
//! reach those sibling services use `proxy_core`; routes that reach
//! the public internet go through the (wave-2) egress module instead.
//!
//! Public surface:
//!   * [`pipe_action`] — fire a pipe action and translate the
//!     IPC error envelope into an `(http_status, envelope)` response.
//!   * [`validate_token`] — auth helper called by routes; wraps
//!     `services::device_gate::verify` and shapes the error path for
//!     direct return from a handler.
//!   * [`http_call`] — async JSON HTTP call to a localhost sibling.
//!     Mirrors the Python `httpx.AsyncClient` wrapper but built on
//!     `reqwest::Client`. Returns the upstream body unwrapped on
//!     success; on failure shapes a canonical `{ok:false, error:{…}}`
//!     envelope keyed by upstream status.

use std::sync::OnceLock;
use std::time::Duration;

use axum::http::StatusCode;
use reqwest::Client;
use serde_json::{json, Value};
use wylde_shared::ipc::{call_action, IpcError};

use crate::services::device_gate;

/// Output of any wave-1 dispatch. `Ok` carries the successful payload,
/// `Err` carries the `(http_status, envelope)` pair the handler should
/// return verbatim.
pub type ProxyResult = Result<Value, (StatusCode, Value)>;

/// Resolve `Authorization: Bearer <token>` to a device record.
///
/// Routes that require authentication will call this once at entry; the
/// wave-1 surface doesn't actually wire auth yet (only `/health` is
/// public-tier and `/health` skips auth by design), but the helper is
/// here so wave 2 can plug it in without touching `services::*`.
pub async fn validate_token(token: &str) -> ProxyResult {
    if token.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            json!({
                "ok": false,
                "error": {
                    "code": "missing_token",
                    "message": "Authorization header is required",
                }
            }),
        ));
    }
    device_gate::verify(token).await
}

/// Fire `action` on `service` over the named-pipe transport and shape
/// the result into the wave-1 dispatch envelope.
pub async fn pipe_action(service: &str, action: &str, payload: Value) -> ProxyResult {
    match call_action(service, action, payload).await {
        Ok(data) => Ok(data),
        Err(err) => Err(map_pipe_error(&err)),
    }
}

fn map_pipe_error(err: &IpcError) -> (StatusCode, Value) {
    let status = match err.code.as_str() {
        "pipe_unavailable" | "pipe_connect" => StatusCode::SERVICE_UNAVAILABLE,
        "pipe_timeout" | "handshake_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "not_found" => StatusCode::NOT_FOUND,
        "bad_request" => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    let body = json!({
        "ok": false,
        "error": {
            "code": err.code,
            "message": err.message,
        }
    });
    (status, body)
}

/// Default timeout for [`http_call`]. Matches Python's
/// `proxy_core.DEFAULT_TIMEOUT = 30.0`.
pub const HTTP_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of a successful [`http_call`]: the upstream HTTP status and
/// the parsed body. On failure, the same `(StatusCode, Value)` envelope
/// shape that the pipe transport uses.
pub type HttpResult = Result<(StatusCode, Value), (StatusCode, Value)>;

/// Process-wide reqwest client. Built lazily on first use so test
/// binaries that never hit [`http_call`] don't pay the cost.
fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            // Localhost daemons; no proxy, no redirects (Ollama doesn't
            // emit any). `tcp_keepalive` keeps the connection pool warm
            // between back-to-back calls.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .expect("reqwest::Client::build cannot fail with these options")
    })
}

/// HTTP method for [`http_call`]. Mirrors the Python `method` string but
/// typed so handlers can't pass a malformed verb.
#[derive(Clone, Copy, Debug)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

impl HttpMethod {
    /// Convert to the underlying `reqwest::Method`. Public so the
    /// streaming module — which builds requests directly off the
    /// shared client — can reuse the same verb mapping without a
    /// duplicate match.
    pub fn into_reqwest(self) -> reqwest::Method {
        match self {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
        }
    }
}

/// Streaming-friendly variant of [`shared_client`]. Built without the
/// outer-request timeout so the per-chunk timeout in `streaming.rs`
/// owns liveness end-to-end. Used only by [`crate::streaming`].
pub fn streaming_client() -> &'static Client {
    static STREAMING_CLIENT: OnceLock<Client> = OnceLock::new();
    STREAMING_CLIENT.get_or_init(|| {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .tcp_keepalive(Duration::from_secs(30))
            // No global timeout — the NDJSON loop applies a per-chunk
            // timeout via `tokio::time::timeout`. A long Ollama pull
            // legitimately runs for minutes.
            .build()
            .expect("reqwest::Client::build cannot fail with these options")
    })
}

/// Async JSON HTTP call to a localhost sibling service.
///
/// Rust analog of `Gateway/proxy_core.py::http_call`. On a 2xx response
/// the parsed JSON body (or a `{"text": "…"}` wrapper if upstream
/// returned non-JSON) is returned with the upstream status. On a
/// non-2xx response or a transport error, a canonical failure envelope
/// is returned in the `Err` arm — exactly the same shape pipe handlers
/// produce, so route code can fold both transports through one match.
///
/// `body` is sent as JSON when `Some`. `timeout` is applied to the
/// whole request; default is [`HTTP_DEFAULT_TIMEOUT`].
pub async fn http_call(
    url: &str,
    method: HttpMethod,
    body: Option<Value>,
    timeout: Duration,
) -> HttpResult {
    let mut req = shared_client()
        .request(method.into_reqwest(), url)
        .timeout(timeout);
    if let Some(b) = body {
        req = req.json(&b);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        // Connect failures (refused / unreachable) and request body
        // errors are transport errors regardless of the timeout
        // wrapper — categorize them first so a connection-refused
        // doesn't accidentally surface as a 504 just because the
        // per-request timeout was short.
        Err(e) if e.is_connect() || e.is_request() => {
            return Err((
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "error": {
                        "code": "transport",
                        "message": format!("{url}: {e}"),
                    }
                }),
            ));
        }
        Err(e) if e.is_timeout() => {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                json!({
                    "ok": false,
                    "error": {
                        "code": "timeout",
                        "message": format!(
                            "{url} did not respond within {}s",
                            timeout.as_secs_f64()
                        ),
                    }
                }),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "error": {
                        "code": "transport",
                        "message": format!("{url}: {e}"),
                    }
                }),
            ));
        }
    };

    let upstream_status = resp.status();
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "error": {
                        "code": "transport",
                        "message": format!("{url}: read body: {e}"),
                    }
                }),
            ));
        }
    };

    let body_value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(_) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
        }
    };

    if upstream_status.is_success() {
        return Ok((status, body_value));
    }

    // Non-2xx — mirror Python's `error("http_<status>", ...)` shape.
    let code = format!("http_{}", upstream_status.as_u16());
    let message = match &body_value {
        Value::Object(map) => map
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| map.get("message").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| truncate(&body_value.to_string(), 300)),
        Value::String(s) => truncate(s, 300),
        _ => upstream_status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_owned(),
    };

    let mut err = json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
        }
    });
    if let Value::Object(map) = &body_value {
        err["error"]["details"] = Value::Object(map.clone());
    }
    Err((status, err))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        s[..end].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validate_empty_token_is_401() {
        let res = validate_token("").await;
        let (status, body) = res.expect_err("empty token should fail");
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "missing_token");
    }

    #[test]
    fn map_pipe_unavailable_is_503() {
        let (s, _) = map_pipe_error(&IpcError::new("pipe_unavailable", "no pipe"));
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn map_pipe_timeout_is_504() {
        let (s, _) = map_pipe_error(&IpcError::new("pipe_timeout", "deadline"));
        assert_eq!(s, StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn map_unknown_is_502() {
        let (s, _) = map_pipe_error(&IpcError::new("nope", "huh"));
        assert_eq!(s, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn truncate_under_limit_returns_input() {
        assert_eq!(truncate("hello", 300), "hello");
    }

    #[test]
    fn truncate_long_string_caps_length() {
        let s = "x".repeat(500);
        let out = truncate(&s, 300);
        assert_eq!(out.len(), 300);
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        // 3-byte UTF-8 codepoints — naive byte slicing at byte 100 would
        // land mid-codepoint. The helper must back off to the previous
        // boundary.
        let s = "字".repeat(50); // 150 bytes
        let out = truncate(&s, 100);
        // Every Chinese codepoint is 3 bytes; 100 / 3 = 33 chars = 99 bytes.
        assert_eq!(out.len(), 99);
        // Must still be valid UTF-8.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn http_call_transport_error_on_closed_port() {
        // Bind+drop to get a port we're certain nothing answers on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/api/tags");
        // Use a generous timeout so the OS has time to surface
        // ECONNREFUSED — on Windows the TCP stack may retry SYN
        // packets before failing fast, and a 500ms ceiling can trip
        // the timeout branch instead of the connect-refused branch.
        let res = http_call(&url, HttpMethod::Get, None, Duration::from_secs(5)).await;
        let (status, body) = res.expect_err("closed-port call must error");
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "transport");
    }

    #[tokio::test]
    async fn http_call_success_returns_upstream_status_and_body() {
        // Spin up a one-shot HTTP/1.1 server that returns 200 + JSON.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                // Test scaffolding: one-shot mock HTTP server. Read /
                // write / shutdown errors just fail the assertion
                // below, so discard the Result.
                let _ = sock.read(&mut buf).await; // wylde-check: discard-result-ok
                let body = br#"{"models":[{"name":"llama3"}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await; // wylde-check: discard-result-ok
                let _ = sock.write_all(body).await; // wylde-check: discard-result-ok
                let _ = sock.shutdown().await; // wylde-check: discard-result-ok
            }
        });

        let url = format!("http://{addr}/api/tags");
        let (status, body) = http_call(&url, HttpMethod::Get, None, Duration::from_secs(2))
            .await
            .expect("200 response must succeed");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["models"][0]["name"], "llama3");
        handle.await.ok(); // wylde-check: discard-result-ok
    }

    #[tokio::test]
    async fn http_call_non_2xx_returns_canonical_failure_envelope() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await; // wylde-check: discard-result-ok
                let body = br#"{"error":"model not found"}"#;
                let resp = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await; // wylde-check: discard-result-ok
                let _ = sock.write_all(body).await; // wylde-check: discard-result-ok
                let _ = sock.shutdown().await; // wylde-check: discard-result-ok
            }
        });

        let url = format!("http://{addr}/api/delete");
        let res = http_call(
            &url,
            HttpMethod::Delete,
            Some(json!({"name": "ghost"})),
            Duration::from_secs(2),
        )
        .await;
        let (status, env) = res.expect_err("404 must surface as Err");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(env["ok"], false);
        assert_eq!(env["error"]["code"], "http_404");
        assert_eq!(env["error"]["message"], "model not found");
        handle.await.ok(); // wylde-check: discard-result-ok
    }
}
