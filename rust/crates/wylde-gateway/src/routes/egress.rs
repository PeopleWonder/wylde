//! `/api/egress/*` — outbound HTTP proxy surface.
//!
//! Rust port of `Gateway/routes/egress.py`. Four endpoints:
//!
//! * `GET  /api/egress/destinations` — list configured upstreams (no
//!   secret values, only env-var names).
//! * `POST /api/egress/kill`        — toggle / read the kill switch.
//! * `POST /api/egress/forward`     — unary outbound call.
//! * `POST /api/egress/stream`      — chunked / NDJSON outbound call.
//!
//! Auth: every endpoint gates on `require_local` (CIDR-based) — the
//! loopback + WyldeLink CGNAT tier, matching the Python `egress.py`.
//!
//! ## Wire format
//!
//! `destinations`, `kill`, and `forward` return the canonical
//! `{ok: true, data}` envelope. **`/api/egress/stream` deliberately
//! bypasses the envelope**: it pipes the upstream's raw byte stream
//! (SSE / NDJSON / chunked) straight through with the upstream
//! `Content-Type`, so there is no JSON body to wrap. This is
//! intentional and was confirmed during the Bucket-A IPC cleanup; the
//! only other raw route in the gateway is `/api/link/qr/:token` (raw
//! SVG image).

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Json;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::require_local;
use crate::egress::{
    self,
    client::{forward, forward_stream},
};
use crate::envelopes::{failure, success};

#[derive(Debug, Deserialize, Default)]
pub struct ForwardRequest {
    #[serde(default)]
    pub caller: String,
    #[serde(default)]
    pub dest: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default = "default_timeout")]
    pub timeout: f64,
}

fn default_method() -> String {
    "GET".into()
}
fn default_path() -> String {
    "/".into()
}
fn default_timeout() -> f64 {
    30.0
}

#[derive(Debug, Deserialize, Default)]
pub struct StreamRequest {
    #[serde(default)]
    pub caller: String,
    #[serde(default)]
    pub dest: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: f64,
}

fn default_connect_timeout() -> f64 {
    10.0
}

#[derive(Debug, Deserialize, Default)]
pub struct KillSwitchRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
}

// ── Routes ─────────────────────────────────────────────────────────────

/// `GET /api/egress/destinations` — list every per-component destination.
pub async fn get_destinations() -> Response {
    success(json!({
        "destinations": egress::list_destinations(),
        "kill_switch_engaged": egress::is_blocked(),
    }))
}

/// `POST /api/egress/kill` — toggle / read the egress kill switch.
pub async fn kill_switch(body: Option<Json<KillSwitchRequest>>) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    if let Some(enabled) = req.enabled {
        egress::set_blocked(enabled);
    }
    success(json!({ "engaged": egress::is_blocked() }))
}

/// `POST /api/egress/forward` — unary outbound call.
pub async fn forward_route(body: Option<Json<ForwardRequest>>) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let timeout = Duration::from_secs_f64(req.timeout.max(0.001));
    match forward(
        &req.caller,
        &req.dest,
        &req.method,
        &req.path,
        req.body.as_ref(),
        req.headers.as_ref(),
        timeout,
    )
    .await
    {
        Ok(result) => success(json!({
            "status": result.status,
            "headers": result.headers,
            "body": result.body,
            "duration_ms": (result.duration_ms * 1000.0).round() / 1000.0,
        })),
        Err(e) => egress_error_to_response(e),
    }
}

/// `POST /api/egress/stream` — chunked / NDJSON outbound call.
pub async fn stream_route(body: Option<Json<StreamRequest>>) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let connect_timeout = Duration::from_secs_f64(req.connect_timeout.max(0.001));
    match forward_stream(
        &req.caller,
        &req.dest,
        &req.method,
        &req.path,
        req.body.as_ref(),
        req.headers.as_ref(),
        connect_timeout,
    )
    .await
    {
        Ok((status, upstream_headers, byte_stream)) => {
            let mut response = Response::builder().status(status);
            let mut content_type: Option<HeaderValue> = None;
            let drop_headers = [
                "connection",
                "keep-alive",
                "transfer-encoding",
                "content-length",
                "content-encoding",
            ];
            for (k, v) in &upstream_headers {
                if drop_headers.iter().any(|d| k.eq_ignore_ascii_case(d)) {
                    continue;
                }
                if let Ok(val) = HeaderValue::from_str(v) {
                    if k.eq_ignore_ascii_case("content-type") {
                        content_type = Some(val.clone());
                    }
                    response = response.header(k, val);
                }
            }
            // Sensible defaults for SSE-style consumers.
            if !upstream_headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("cache-control"))
            {
                response = response.header("Cache-Control", "no-cache");
            }
            if !upstream_headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("x-accel-buffering"))
            {
                response = response.header("X-Accel-Buffering", "no");
            }
            if content_type.is_none() {
                response = response.header("Content-Type", "application/x-ndjson");
            }
            let status_for_log = status;
            match response.body(Body::from_stream(byte_stream)) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "egress stream: failed to build response (status={status_for_log}): {e}"
                    );
                    failure(
                        "egress_upstream_error",
                        &format!("{e}"),
                        StatusCode::BAD_GATEWAY,
                    )
                }
            }
        }
        Err(e) => egress_error_to_response(e),
    }
}

fn egress_error_to_response(e: crate::egress::client::EgressError) -> Response {
    use crate::egress::client::EgressError;
    match e {
        EgressError::Blocked => failure(
            "egress_blocked",
            "egress kill switch is engaged",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        EgressError::Denied(msg) => failure("egress_denied", &msg, StatusCode::FORBIDDEN),
        EgressError::Policy(msg) => failure("egress_denied", &msg, StatusCode::FORBIDDEN),
        EgressError::Ssrf(msg) => failure("egress_denied", &msg, StatusCode::FORBIDDEN),
        EgressError::Upstream(msg) => {
            failure("egress_upstream_error", &msg, StatusCode::BAD_GATEWAY)
        }
    }
}

/// Build the `/api/egress` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/egress/destinations",
            get(get_destinations).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/egress/kill",
            post(kill_switch).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/egress/forward",
            post(forward_route).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/egress/stream",
            post(stream_route).route_layer(from_fn(require_local)),
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
                Some(b) => Body::from(b.to_owned()),
                None => Body::empty(),
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
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn destinations_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/egress/destinations", None).await;
    }

    #[tokio::test]
    async fn kill_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/egress/kill", Some("{}")).await;
    }

    #[tokio::test]
    async fn forward_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/egress/forward",
            Some(r#"{"caller":"X","dest":"y"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn stream_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/egress/stream",
            Some(r#"{"caller":"X","dest":"y"}"#),
        )
        .await;
    }
}
