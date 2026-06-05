//! `/api/devices` — device_gate management surface.
//!
//! Rust port of `Gateway/routes/devices.py`. Two audiences:
//!
//! * **Desktop GUI** (loopback caller) — pairs new devices, lists
//!   pairings, changes tiers, rotates tokens, revokes.
//! * **Mobile** (CGNAT-tunneled, Bearer-authed) — introspects its own
//!   record at `/api/devices/me` so the app can show the current tier
//!   and last-seen timestamp.
//!
//! ## Wire format
//!
//! Every `/api/devices` route wraps its action reply in the canonical
//! `{ok: true, data: <reply>}` envelope via [`success`], the same shape
//! every other harness/gateway JSON route uses. (The original Python
//! `devices.py` returned the action reply verbatim — e.g. `/api/devices`
//! emitted `{"devices": [...], "count": N}` with no outer wrapper — but
//! that inconsistency was retired in the Bucket-A IPC cleanup so callers
//! see one envelope across the whole surface.) The failure shape is the
//! canonical `{ok: false, error: {code, message}}` (matches Python's
//! `services/device_gate.py::_call_action` error envelope).
//!
//! ## Auth
//!
//! The loopback management routes gate on `require_local` (CIDR-based);
//! `/api/devices/me` gates on `require_device`, which resolves the
//! caller's Bearer token to a verified device record. Same split the
//! Python `devices.py` uses.

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use serde_json::{json, Value};

use crate::auth::{require_device, require_local, Device};
use crate::envelopes::{failure, success};
use crate::middleware::{device_limiter, forward_device_events, per_device_rate_limit};
use crate::services::device_gate as svc;

/// `GET /api/devices` — list every registered device.
pub async fn list_all() -> Response {
    finish(svc::list_devices().await)
}

/// `POST /api/devices/pairing/start` — open a fresh pairing window.
pub async fn start_pairing() -> Response {
    finish(svc::start_pairing().await)
}

/// `POST /api/devices/pairing/cancel` — close the current pairing window.
pub async fn cancel_pairing() -> Response {
    finish(svc::cancel_pairing().await)
}

/// `GET /api/devices/pairing/status` — report the active pairing window.
pub async fn pairing_status() -> Response {
    finish(svc::get_pairing_status().await)
}

/// `POST /api/devices/:device_id/tier` — change a device's tier.
pub async fn set_tier(Path(device_id): Path<String>, body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let tier = payload
        .get("tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if tier.is_empty() {
        return failure("bad_request", "tier is required", StatusCode::BAD_REQUEST);
    }
    finish(svc::set_tier(&device_id, &tier).await)
}

/// `POST /api/devices/:device_id/rotate` — rotate a device token.
pub async fn rotate_token(Path(device_id): Path<String>) -> Response {
    finish(svc::rotate_token(&device_id).await)
}

/// `DELETE /api/devices/:device_id` — revoke a device.
pub async fn revoke(Path(device_id): Path<String>) -> Response {
    finish(svc::revoke(&device_id).await)
}

/// `GET /api/devices/me` — return the authenticated device's own record.
///
/// Mobile uses this to display its current tier in the settings page
/// without poking every potentially-blocked surface. The
/// `X-Wylde-Events` header is handled by the [`forward_device_events`]
/// layer mounted on this route, not the handler itself.
pub async fn me(Extension(device): Extension<Device>) -> Response {
    success(json!({"device_id": device.device_id, "tier": device.tier}))
}

/// Translate the [`svc::GateResult`] tuple into the canonical wire
/// format: success wraps the action data in `{ok: true, data}`; failure
/// returns the canonical `{ok: false, error: {code, message}}`.
fn finish(result: svc::GateResult) -> Response {
    match result {
        Ok(data) => success(data),
        Err((status, body)) => {
            // services::device_gate already produced the canonical
            // failure envelope; pass it through with the upstream status.
            (status, axum::Json(body)).into_response()
        }
    }
}

/// Build the `/api/devices` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/devices",
            get(list_all).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/devices/pairing/start",
            post(start_pairing).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/devices/pairing/cancel",
            post(cancel_pairing).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/devices/pairing/status",
            get(pairing_status).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/devices/me",
            get(me)
                // Both per-route layers run inner to `require_device`
                // (the `Device` extension is already populated).
                // `forward_device_events` drains the device's pending-
                // event queue onto the `X-Wylde-Events` response header;
                // `per_device_rate_limit` caps requests per device.
                // The rate-limit layer sits outer to events so a 429'd
                // request leaves the pending-event queue intact.
                .route_layer(from_fn(forward_device_events))
                .route_layer(from_fn_with_state(device_limiter(), per_device_rate_limit))
                .route_layer(from_fn(require_device)),
        )
        .route(
            "/api/devices/:device_id/tier",
            post(set_tier).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/devices/:device_id/rotate",
            post(rotate_token).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/devices/:device_id",
            delete(revoke).route_layer(from_fn(require_local)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    /// Drive a GUI (loopback-tier) route from a non-local caller; assert
    /// the canonical `403 auth_local_denied` envelope `require_local`
    /// emits.
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
    async fn list_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/devices", None).await;
    }

    #[tokio::test]
    async fn start_pairing_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/devices/pairing/start", None).await;
    }

    #[tokio::test]
    async fn cancel_pairing_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/devices/pairing/cancel", None).await;
    }

    #[tokio::test]
    async fn pairing_status_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/devices/pairing/status", None).await;
    }

    #[tokio::test]
    async fn set_tier_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/devices/dev-1/tier",
            Some(r#"{"tier":"local"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn rotate_token_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/devices/dev-1/rotate", None).await;
    }

    #[tokio::test]
    async fn revoke_rejects_non_local_caller() {
        assert_local_denied("DELETE", "/api/devices/dev-1", None).await;
    }

    /// `/api/devices/me` is the one device-tier route here: it needs a
    /// Bearer token, so a request without one is `401 missing_token` —
    /// not the `403` the loopback-tier GUI routes give a non-local
    /// caller.
    #[tokio::test]
    async fn me_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/devices/me")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "missing_token");
    }

    #[tokio::test]
    async fn success_wraps_action_data_in_envelope() {
        // Verify the bytes carry the canonical `{ok: true, data}` wrapper
        // — the Bucket-A IPC cleanup brought /api/devices in line with the
        // rest of the JSON surface.
        let resp = success(json!({"devices": [], "count": 0}));
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["devices"], json!([]));
        assert_eq!(v["data"]["count"], 0);
    }
}
