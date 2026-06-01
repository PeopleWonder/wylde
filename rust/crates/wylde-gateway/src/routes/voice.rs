//! `/api/voice` — voice command + STT/TTS proxy.
//!
//! Rust port of `Gateway/routes/voice.py`. Mobile captures audio
//! locally, POSTs it base64-encoded to `/api/voice/transcribe`, gets
//! text back, optionally fires `/api/voice/command` on the desktop, and
//! asks `/api/voice/speak` to TTS a string. All four routes proxy
//! through the named pipe to `\\.\pipe\wylde-voice` — no separate HTTP
//! listener.
//!
//! ## Wire format vs Python
//!
//! Python's `voice.py` uses `pipe_call(service, "/api/...", http_verb=…)`
//! — the Flask-style pipe surface. The Python docstring itself flags
//! that the live Voice pipe is action-based (`voice.toggle`,
//! `voice.subscribe_status`, …) and these Flask-style proxies need to
//! be repointed at action envelopes "for the routes to fully wire
//! end-to-end". Until that repoint lands on the Voice side, both the
//! Python and Rust routes hit a Flask stub that only registers
//! `/health`; the body endpoints (`/api/command`, `/api/speak`,
//! `/api/listen`) 404 in production today.
//!
//! The Rust port uses [`wylde_shared::ipc::send`] (Flask-style pipe
//! frame, POST-only) to match Python's wire bytes for the three POST
//! routes. The `/health` GET route is a deviation: `ipc::send`
//! hardcodes `http_verb="POST"`, so this port sends POST `/health`
//! against the Voice Flask stub, which returns 405 instead of Python's
//! 200. This is a known wave-2d limitation pending either an
//! `ipc::send_with_verb` helper or the action-style repoint described
//! above — neither is in wave-2d scope (route porting only).
//!
//! ## Auth: `require_local`
//!
//! Both Python and this port gate the voice routes on `require_local`
//! — the CIDR allowlist (loopback + WyldeLink CGNAT). A tunneled
//! mobile peer reaches the Gateway as a 100.64/10 caller and passes.

use std::time::Duration;

use axum::extract::Json;
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;
use wylde_shared::ipc::{send, IpcError, Reply};

use crate::auth::require_local;
use crate::envelopes::{failure, success};

/// `\\.\pipe\wylde-voice` service name. Matches Python's `VOICE_PIPE`.
const VOICE_PIPE: &str = "wylde-voice";

/// 60s pipe timeout, matches Python's `timeout=60.0` on the three body
/// routes. The /health route uses a separate 5s timeout below.
const BODY_TIMEOUT: Duration = Duration::from_secs(60);

/// 5s pipe timeout for /health, matches Python's `timeout=5.0`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// `POST /api/voice/command` — execute a voice-driven harness command.
pub async fn command(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    forward_pipe("/api/command", payload, BODY_TIMEOUT).await
}

/// `POST /api/voice/speak` — TTS a string via the Voice service.
pub async fn speak(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    forward_pipe("/api/speak", payload, BODY_TIMEOUT).await
}

/// `POST /api/voice/transcribe` — STT base64 audio to text.
pub async fn transcribe(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    forward_pipe("/api/listen", payload, BODY_TIMEOUT).await
}

/// `GET /api/voice/health` — proxy the Voice service's /health.
///
/// See module docstring re POST/GET verb mismatch against the Python
/// Voice Flask stub.
pub async fn health() -> Response {
    forward_pipe("/health", Value::Null, HEALTH_TIMEOUT).await
}

/// Shared pipe-call path: `wylde_shared::ipc::send` returns a
/// [`Reply`] whose `ok` flag maps onto a standard `{ok, data|error}`
/// envelope. Pipe transport failures land in `Reply::error` and bubble
/// up as 502/503/504 per error code, mirroring
/// `proxy_core::pipe_action`'s `IpcError` → HTTP map.
async fn forward_pipe(method: &str, data: Value, timeout: Duration) -> Response {
    let reply: Reply = send(VOICE_PIPE, method, data, timeout).await;
    if reply.ok {
        return success(reply.data);
    }
    let err = reply
        .error
        .unwrap_or_else(|| IpcError::new("unknown", "voice pipe call failed"));
    let status = err_to_status(&err.code);
    let message = if err.message.is_empty() {
        "voice pipe call failed".to_owned()
    } else {
        err.message
    };
    failure(&err.code, &message, status)
}

fn err_to_status(code: &str) -> StatusCode {
    match code {
        "pipe_unavailable" | "pipe_connect" | "ipc_disabled" => StatusCode::SERVICE_UNAVAILABLE,
        "pipe_timeout" | "handshake_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "not_found" => StatusCode::NOT_FOUND,
        "bad_request" => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// Build the `/api/voice` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/voice/command",
            post(command).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/voice/speak",
            post(speak).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/voice/transcribe",
            post(transcribe).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/voice/health",
            get(health).route_layer(from_fn(require_local)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    /// Drive a route from a non-local caller; assert the canonical
    /// `403 auth_local_denied` envelope the `require_local` tier emits.
    async fn assert_local_denied(method: &str, uri: &str, body: Option<&str>) {
        let app = router();
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let mut request = req
            .body(match body {
                Some(b) => axum::body::Body::from(b.to_owned()),
                None => axum::body::Body::empty(),
            })
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} should 403 for a non-local caller"
        );
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn command_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/voice/command", Some(r#"{"text":"hi"}"#)).await;
    }

    #[tokio::test]
    async fn speak_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/voice/speak", Some(r#"{"text":"hi"}"#)).await;
    }

    #[tokio::test]
    async fn transcribe_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/voice/transcribe",
            Some(r#"{"audio_b64":"…"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn health_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/voice/health", None).await;
    }

    #[test]
    fn err_to_status_maps_canonical_codes() {
        assert_eq!(
            err_to_status("pipe_unavailable"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            err_to_status("pipe_connect"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            err_to_status("ipc_disabled"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(err_to_status("pipe_timeout"), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            err_to_status("handshake_timeout"),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(err_to_status("not_found"), StatusCode::NOT_FOUND);
        assert_eq!(err_to_status("bad_request"), StatusCode::BAD_REQUEST);
        assert_eq!(err_to_status("transport"), StatusCode::BAD_GATEWAY);
        assert_eq!(err_to_status("unknown"), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn voice_constants_match_python() {
        // VOICE_PIPE in Python is "wylde-voice"; BODY_TIMEOUT is 60s;
        // HEALTH_TIMEOUT is 5s. Locks these to catch accidental drift.
        assert_eq!(VOICE_PIPE, "wylde-voice");
        assert_eq!(BODY_TIMEOUT, Duration::from_secs(60));
        assert_eq!(HEALTH_TIMEOUT, Duration::from_secs(5));
    }
}
