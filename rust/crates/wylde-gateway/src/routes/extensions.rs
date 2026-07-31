//! `/extensions/{name}/{endpoint}` — browser-extension dispatch surface.
//!
//! Rust port of `Gateway/routes/extensions.py`. Browser extensions POST
//! (or GET) to `/extensions/<name>/<endpoint>`; the call is forwarded
//! to the extension dispatcher over `\\.\pipe\wylde-extension-bridge`.
//!
//! The extension bridge's dispatch logic is in-process Python
//! (`Extensions/extension_bridge/`); the `wylde-extension-bridge` pipe
//! service ([`Extensions/extension_bridge/pipe.py`]) wraps it so this
//! Rust port — which has no in-process Python — reaches the same
//! upstream the Python Gateway does. Both Gateways therefore emit
//! byte-identical envelopes from the same dispatcher.
//!
//! ## Error mapping
//!
//! Matches `Gateway/services/extensions.py::dispatch`:
//!
//! | bridge error code     | status | envelope code                  |
//! |-----------------------|--------|--------------------------------|
//! | `extension_not_found` | 404    | `extension_not_found`          |
//! | `extension_disabled`  | 409    | `extension_disabled`           |
//! | `extension_error`     | 500    | `extension_error`              |
//! | *(pipe transport)*    | 503    | `extension_bridge_unavailable` |
//!
//! A pipe-transport failure (bridge service down, action unregistered,
//! decode error) folds onto `extension_bridge_unavailable` so a Gateway
//! running without the bridge service still produces the canonical 503
//! the Python side produces from the same condition.
//!
//! ## Auth
//!
//! Both Python and this port gate the route on `require_local` — the
//! CIDR allowlist (loopback + WyldeLink CGNAT).

use std::collections::HashMap;

use axum::extract::{Json, Path, Query};
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

use crate::auth::require_local;
use crate::envelopes::{failure, success};
use crate::proxy_core::pipe_action;

/// `\\.\pipe\wylde-extension-bridge` service name. Matches Python's
/// `extension_routes.BRIDGE_SERVICE`.
const BRIDGE_SERVICE: &str = "wylde-extension-bridge";

/// A bridge-dispatch failure already mapped onto its HTTP shape.
pub(crate) struct BridgeFailure {
    pub code: String,
    pub message: String,
    pub status: StatusCode,
}

/// Dispatch one extension call through the `wylde-extension-bridge`
/// pipe. On success returns the handler's raw result; on failure
/// returns a [`BridgeFailure`] with transport faults folded onto
/// `extension_bridge_unavailable`.
///
/// Shared by the HTTP route here and `pipe::handle_extensions_dispatch`
/// so both surfaces map the bridge's error codes identically.
pub(crate) async fn dispatch_through_bridge(
    name: &str,
    endpoint: &str,
    params: Value,
) -> Result<Value, BridgeFailure> {
    let payload = json!({
        "extension": name,
        "endpoint": endpoint,
        "params": params,
    });
    match pipe_action(BRIDGE_SERVICE, "extensions.dispatch", payload).await {
        Ok(data) => Ok(data),
        Err((_, body)) => {
            let code = body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let message = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let (mapped, status) = map_bridge_code(&code);
            let message = if message.is_empty() {
                "extension bridge call failed".to_owned()
            } else {
                message
            };
            Err(BridgeFailure {
                code: mapped.to_owned(),
                message,
                status,
            })
        }
    }
}

/// Map a bridge / transport error code onto the canonical envelope
/// code + HTTP status. Anything that is not one of the three
/// structured bridge codes is treated as a transport fault → 503.
fn map_bridge_code(code: &str) -> (&'static str, StatusCode) {
    match code {
        "extension_not_found" => ("extension_not_found", StatusCode::NOT_FOUND),
        "extension_disabled" => ("extension_disabled", StatusCode::CONFLICT),
        "extension_error" => ("extension_error", StatusCode::INTERNAL_SERVER_ERROR),
        _ => (
            "extension_bridge_unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    }
}

async fn dispatch_response(name: &str, endpoint: &str, params: Value) -> Response {
    match dispatch_through_bridge(name, endpoint, params).await {
        Ok(data) => success(data),
        Err(f) => failure(&f.code, &f.message, f.status),
    }
}

/// `POST /extensions/{name}/{endpoint}` — JSON body carries the params.
async fn call_extension_post(
    Path((name, endpoint)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Response {
    // Mirrors Python `_read_params`: an object body is the params map,
    // a non-object body is wrapped under `_raw`, no body is `{}`.
    let params = match body {
        Some(Json(Value::Object(m))) => Value::Object(m),
        Some(Json(other)) => json!({ "_raw": other }),
        None => json!({}),
    };
    dispatch_response(&name, &endpoint, params).await
}

/// `GET /extensions/{name}/{endpoint}` — query string carries the params.
async fn call_extension_get(
    Path((name, endpoint)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Mirrors Python `_read_params`: `dict(request.query_params)`.
    let params = Value::Object(
        params
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    );
    dispatch_response(&name, &endpoint, params).await
}

pub fn router() -> Router {
    Router::new().route(
        "/extensions/{name}/{endpoint}",
        post(call_extension_post)
            .get(call_extension_get)
            .route_layer(from_fn(require_local)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    #[tokio::test]
    async fn post_rejects_non_local_caller() {
        let app = router();
        let mut req = Request::builder()
            .method("POST")
            .uri("/extensions/foo/bar")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn get_verb_is_registered() {
        // A local caller on the GET surface must reach the dispatch
        // handler — not axum's 405 (which is what an unregistered verb
        // on a mounted path returns). With no bridge service up the
        // handler resolves to a 503/4xx envelope; the point of this
        // test is only that `.get()` is wired.
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/extensions/foo/bar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn map_bridge_code_covers_structured_and_transport() {
        assert_eq!(
            map_bridge_code("extension_not_found"),
            ("extension_not_found", StatusCode::NOT_FOUND)
        );
        assert_eq!(
            map_bridge_code("extension_disabled"),
            ("extension_disabled", StatusCode::CONFLICT)
        );
        assert_eq!(
            map_bridge_code("extension_error"),
            ("extension_error", StatusCode::INTERNAL_SERVER_ERROR)
        );
        // Transport-layer codes all fold onto the canonical 503.
        for code in ["pipe_connect", "pipe_unavailable", "no_action", ""] {
            assert_eq!(
                map_bridge_code(code),
                (
                    "extension_bridge_unavailable",
                    StatusCode::SERVICE_UNAVAILABLE
                )
            );
        }
    }

    #[tokio::test]
    async fn dispatch_through_bridge_errors_for_unknown_extension() {
        // No bridge service is part of the unit-test fixture, so an
        // unknown extension always resolves to an `Err`: a 503
        // `extension_bridge_unavailable` when the pipe is down, or a
        // 404 `extension_not_found` if a live bridge answers. Either is
        // a mapped `BridgeFailure`, never a panic.
        let res = dispatch_through_bridge("wylde-parity-probe", "probe", json!({})).await;
        let f = res.expect_err("unknown extension must error");
        assert!(
            f.code == "extension_bridge_unavailable" || f.code == "extension_not_found",
            "unexpected code: {}",
            f.code
        );
        assert!(f.status.is_client_error() || f.status.is_server_error());
    }

    #[tokio::test]
    async fn failure_envelope_shape() {
        // The wire shape a mapped failure produces — the canonical
        // `{ok: false, error: {code, message}}` envelope.
        let resp = failure(
            "extension_bridge_unavailable",
            "extension bridge call failed",
            StatusCode::SERVICE_UNAVAILABLE,
        );
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(resp.into_body(), 2048).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "extension_bridge_unavailable");
    }
}
