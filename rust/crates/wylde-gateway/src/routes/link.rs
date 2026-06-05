//! `/api/link` — thin proxy onto the VPN management API.
//!
//! Rust port of `Gateway/routes/link.py`. Mobile apps reach Wylde only
//! via the WyldeLink tunnel; once tunneled in they pass `require_local`
//! (CGNAT 100.64/10). This router gives mobile a Gateway-tier URL for
//! the self-management operations that actually live in the VPN
//! service: status, peers, STUN, pairing, QR.
//!
//! ## Why proxy at all
//!
//! Two reasons (preserved from the Python file's docstring):
//!
//! 1. **Single auth boundary** (Wylde Design Principle #16). Keeps
//!    every mobile-visible URL on one tier; the VPN management port
//!    (127.0.0.1:8020) stays loopback-only.
//! 2. **One audit log.** The Gateway's audit middleware records the
//!    link call alongside every other mobile request.
//!
//! ## Transports used
//!
//! * `/status`, `/peers`, `/peers/remove`, `/stun` — HTTP loopback via
//!   [`proxy_core::http_call`].
//! * `/qr/:token` — raw byte passthrough via [`reqwest::Client`]
//!   directly (the QR endpoint returns `image/svg+xml`; `http_call`
//!   would JSON-decode it and lose the SVG).
//! * `/pair` — named pipe via
//!   [`services::device_gate::complete_pairing`]. Per the Python
//!   note, pair stays on this router (rather than `/api/devices/`)
//!   because mobile reaches it through the WyldeLink tunnel — same
//!   trust boundary as the rest of /api/link.
//!
//! ## Wire format
//!
//! Success responses wrap the upstream body in
//! `{ok: true, data: <upstream-json>}` with the upstream status, same
//! shape Python's `proxy_core.http_call` + `proxy_core.ok` produce.
//! Failure responses use the canonical nested envelope (same
//! cross-wave convention picked by wave 2c).
//!
//! **Deliberate envelope bypass:** `/qr/:token` returns a raw
//! `image/svg+xml` body with no `{ok, data}` wrapper — the QR image is a
//! binary payload, not JSON, so wrapping it would be meaningless. This
//! is intentional and was confirmed during the Bucket-A IPC cleanup;
//! the only other raw route in the gateway is `/api/egress/stream`
//! (raw byte stream).
//!
//! ## Auth
//!
//! Every link route gates on `require_local` (loopback + WyldeLink
//! CGNAT tier), matching the Python `link.py` — except `/pair`, which
//! is deliberately unauthenticated (mobile hits it before it has a
//! token, to exchange a pairing code for one).

use std::time::Duration;

use axum::extract::{Json, Path};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::from_fn;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;

use crate::auth::require_local;
use crate::envelopes::{failure, success_with_status};
use crate::proxy_core::{http_call, HttpMethod};
use crate::services::device_gate as dg;

/// Local VPN management API. Matches Python's `VPN_HTTP`.
const VPN_HTTP: &str = "http://127.0.0.1:8020";

/// 5s default for /status (Python: `timeout=5.0`).
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
/// 10s for /stun (Python: `timeout=10.0`).
const STUN_TIMEOUT: Duration = Duration::from_secs(10);
/// 30s default (matches `proxy_core::HTTP_DEFAULT_TIMEOUT`).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// `GET /api/link/status` — current WyldeLink tunnel state.
pub async fn status() -> Response {
    forward_one_shot(
        &format!("{VPN_HTTP}/api/link/status"),
        HttpMethod::Get,
        None,
        SHORT_TIMEOUT,
    )
    .await
}

/// `GET /api/link/peers` — list paired peers.
pub async fn peers() -> Response {
    forward_one_shot(
        &format!("{VPN_HTTP}/api/link/peers"),
        HttpMethod::Get,
        None,
        DEFAULT_TIMEOUT,
    )
    .await
}

/// `POST /api/link/peers/remove` — remove a paired peer.
pub async fn remove_peer(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v);
    forward_one_shot(
        &format!("{VPN_HTTP}/api/link/peers/remove"),
        HttpMethod::Post,
        payload,
        DEFAULT_TIMEOUT,
    )
    .await
}

/// `GET /api/link/stun` — run a STUN probe and return the public addr.
pub async fn stun() -> Response {
    forward_one_shot(
        &format!("{VPN_HTTP}/api/link/stun"),
        HttpMethod::Get,
        None,
        STUN_TIMEOUT,
    )
    .await
}

/// `POST /api/link/pair` — mobile pairing entry. Body:
/// `{code, username, password, device_metadata}`. Returns
/// `{device_id, token, tier}` on success — mobile stores the token
/// and uses it as the Bearer on every subsequent request.
///
/// Calls `device_gate.complete_pairing` over the pipe (not the VPN
/// HTTP service) to match the Python behaviour.
pub async fn pair(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let code = payload
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let username = payload
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let password = payload
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let metadata = payload
        .get("device_metadata")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    match dg::complete_pairing(&code, &username, &password, metadata).await {
        Ok(data) => (StatusCode::OK, axum::Json(data)).into_response(),
        Err((status, body)) => (status, axum::Json(body)).into_response(),
    }
}

/// `GET /api/link/qr/:token` — raw SVG passthrough.
///
/// Bypasses [`http_call`] because the body isn't JSON; the upstream
/// content type is preserved on the response.
pub async fn qr(Path(token): Path<String>) -> Response {
    if token.trim().is_empty() {
        return failure("bad_request", "token is required", StatusCode::BAD_REQUEST);
    }
    let url = format!("{VPN_HTTP}/api/link/qr/{token}");
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(SHORT_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return failure("transport", &e.to_string(), StatusCode::BAD_GATEWAY),
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return failure("transport", &format!("{url}: {e}"), StatusCode::BAD_GATEWAY);
        }
    };
    let upstream_status = resp.status();
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return failure(
                "transport",
                &format!("{url}: read body: {e}"),
                StatusCode::BAD_GATEWAY,
            );
        }
    };
    let mut response = (status, bytes.to_vec()).into_response();
    if let Ok(hv) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert("content-type", hv);
    }
    response
}

/// Shared one-shot HTTP forward: call upstream, wrap success body in
/// `{ok: true, data}`, fold error into the canonical envelope.
async fn forward_one_shot(
    url: &str,
    method: HttpMethod,
    body: Option<Value>,
    timeout: Duration,
) -> Response {
    match http_call(url, method, body, timeout).await {
        Ok((status, value)) => success_with_status(value, status),
        Err((status, env)) => {
            let code = env
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_owned();
            let message = env
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            failure(&code, &message, status)
        }
    }
}

/// Build the `/api/link` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/link/status",
            get(status).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/link/peers",
            get(peers).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/link/peers/remove",
            post(remove_peer).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/link/stun",
            get(stun).route_layer(from_fn(require_local)),
        )
        .route("/api/link/pair", post(pair))
        .route(
            "/api/link/qr/:token",
            get(qr).route_layer(from_fn(require_local)),
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
    async fn status_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/link/status", None).await;
    }

    #[tokio::test]
    async fn peers_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/link/peers", None).await;
    }

    #[tokio::test]
    async fn remove_peer_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/link/peers/remove",
            Some(r#"{"public_key":"abc"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn stun_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/link/stun", None).await;
    }

    #[tokio::test]
    async fn qr_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/link/qr/sometoken", None).await;
    }

    // /pair is intentionally unauthenticated — mobile hits it *before*
    // it has a token, exchanging a one-time pairing code for one. We
    // verify the bad-request path: empty body must surface from
    // device_gate as a 400 (pipe call to wylde-device-gate; if no
    // pipe is running, will surface as 502/503 — also acceptable).
    #[tokio::test]
    async fn pair_does_not_require_bearer() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/link/pair")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Whatever the result, it must NOT be a 401 "missing_token" —
        // the pair route deliberately skips authorize().
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
