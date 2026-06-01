//! `/api/push` — peer push subscription + drain.
//!
//! Rust port of `Gateway/routes/push.py`. The Phase-9 audit kept
//! `peers.push` (the notification queue + webhook delivery) inside the
//! VPN service so storage stays peer-keyed and process-local. This
//! Gateway-facing route proxies that store over named-pipe IPC:
//! Gateway → `\\.\pipe\wylde-vpn` → `peers.push`.
//!
//! Per Wylde Design Principle #16 the caller's identity for
//! subscribe / unsubscribe / pending is the WireGuard public key the
//! peer announced at tunnel registration — the mobile client sends it
//! explicitly in the request body (`public_key`). The Gateway doesn't
//! know it from request headers because there's no per-request peer
//! credential — the tunnel is the proof.
//!
//! ## Wire format vs Python
//!
//! Same Flask-style pipe surface as `routes::voice` — see that
//! module's docstring for the POST/GET caveat. All three push routes
//! here are POST/GET against `/api/push/{subscribe,unsubscribe,pending}`
//! paths; the live VPN pipe doesn't register those handlers today
//! (`Gateway/routes/push.py` itself is a stub pending the VPN-side
//! repoint), so both Python and Rust 404 from the Flask stub in
//! production. The Rust port mirrors the Python wire bytes via
//! [`wylde_shared::ipc::send`].
//!
//! Error envelopes use the canonical nested shape
//! (`{ok: false, error: {code, message}}`) — same cross-wave
//! convention wave 2c picked over Python's flat
//! `{ok, error, message, code}` form.
//!
//! ## Auth
//!
//! Both Python and this port gate the push routes on `require_local`
//! — the CIDR allowlist (loopback + WyldeLink CGNAT).

use std::time::Duration;

use axum::extract::{Json, Query};
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use wylde_shared::ipc::{send, IpcError, Reply};

use crate::auth::require_local;
use crate::envelopes::{failure, success};

/// `\\.\pipe\wylde-vpn` service name. Matches Python's `VPN_PIPE`.
const VPN_PIPE: &str = "wylde-vpn";

/// 10s pipe timeout, matches Python's `timeout=10.0`.
const PIPE_TIMEOUT: Duration = Duration::from_secs(10);

/// `POST /api/push/subscribe` — register a webhook or poll-mode endpoint.
pub async fn subscribe(body: Option<Json<Value>>) -> Response {
    let payload = match body {
        Some(Json(Value::Object(m))) => Value::Object(m),
        Some(Json(_)) => {
            return failure(
                "bad_request",
                "body must be a JSON object",
                StatusCode::BAD_REQUEST,
            );
        }
        None => {
            return failure(
                "bad_request",
                "public_key required",
                StatusCode::BAD_REQUEST,
            );
        }
    };
    if payload
        .get("public_key")
        .and_then(Value::as_str)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        return failure(
            "bad_request",
            "public_key required",
            StatusCode::BAD_REQUEST,
        );
    }
    forward_pipe("/api/push/subscribe", payload).await
}

/// `POST /api/push/unsubscribe` — remove a subscription.
pub async fn unsubscribe(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    if !payload.is_object()
        || payload
            .get("public_key")
            .and_then(Value::as_str)
            .map(|s| s.is_empty())
            .unwrap_or(true)
    {
        return failure(
            "bad_request",
            "public_key required",
            StatusCode::BAD_REQUEST,
        );
    }
    forward_pipe("/api/push/unsubscribe", payload).await
}

/// Query string carrier for `GET /api/push/pending?public_key=<wg>`.
#[derive(Deserialize)]
pub struct PendingQuery {
    /// WireGuard public key identifying the peer.
    pub public_key: Option<String>,
}

/// `GET /api/push/pending` — drain queued notifications.
///
/// Mobile clients poll this on a timer when they don't have an active
/// push transport (FCM, APNs, etc.). The peer identifies itself via
/// `?public_key=<wg_pubkey>`.
pub async fn pending(Query(q): Query<PendingQuery>) -> Response {
    let key = q.public_key.unwrap_or_default();
    let key = key.trim();
    if key.is_empty() {
        return failure(
            "bad_request",
            "public_key query parameter required",
            StatusCode::BAD_REQUEST,
        );
    }
    forward_pipe("/api/push/pending", json!({"public_key": key})).await
}

async fn forward_pipe(method: &str, data: Value) -> Response {
    let reply: Reply = send(VPN_PIPE, method, data, PIPE_TIMEOUT).await;
    if reply.ok {
        return success(reply.data);
    }
    let err = reply
        .error
        .unwrap_or_else(|| IpcError::new("unknown", "vpn pipe call failed"));
    let status = err_to_status(&err.code);
    let message = if err.message.is_empty() {
        "vpn pipe call failed".to_owned()
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

/// Build the `/api/push` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/push/subscribe",
            post(subscribe).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/push/unsubscribe",
            post(unsubscribe).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/push/pending",
            get(pending).route_layer(from_fn(require_local)),
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
    async fn subscribe_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/push/subscribe",
            Some(r#"{"public_key":"abc"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn unsubscribe_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/push/unsubscribe",
            Some(r#"{"public_key":"abc"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn pending_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/push/pending?public_key=abc", None).await;
    }

    #[test]
    fn err_to_status_maps_canonical_codes() {
        assert_eq!(
            err_to_status("pipe_unavailable"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(err_to_status("pipe_timeout"), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(err_to_status("not_found"), StatusCode::NOT_FOUND);
        assert_eq!(err_to_status("bad_request"), StatusCode::BAD_REQUEST);
        assert_eq!(err_to_status("transport"), StatusCode::BAD_GATEWAY);
    }
}
