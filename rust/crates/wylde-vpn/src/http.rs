//! HTTP control plane — Rust port of `VPN/api.py` (Flask) to axum.
//!
//! Binds `127.0.0.1:8020` (or the configured port) and serves the Flask
//! route set verbatim so Gateway + the GUI keep working with no code
//! changes while the strangler-fig flag is `python`. Every route
//! dispatches to the same action handler the pipe uses, so the HTTP
//! surface is a thin envelope over the action surface.
//!
//! Routes (mirror `VPN/api.py`):
//!
//! * `GET  /health` — process liveness + tunnel state probe.
//! * `GET  /api/vpn/status`            → `vpn.status`
//! * `POST /api/vpn/enable`            → `vpn.enable`   (deferred)
//! * `POST /api/vpn/disable`           → `vpn.disable`  (deferred)
//! * `POST /api/vpn/keygen`            → `vpn.keygen`
//! * `GET  /api/link/status`           → `link.status`
//! * `POST /api/link/pair`             → `link.pair`    (deferred)
//! * `POST /api/link/register`         → `link.register` (deferred)
//! * `GET  /api/link/stun`             → `link.stun`   (deferred)
//! * `GET  /api/link/peers`            → `link.peers`
//! * `POST /api/link/peers/remove`     → `link.peers.remove`
//! * `POST /api/link/connect`          → `link.connect` (deferred)
//! * `GET  /api/link/qr/<token>`       → `link.qr` (returns SVG bytes)
//! * `GET  /api/link/config`           → `link.config.get`
//! * `PATCH /api/link/config`          → `link.config.patch` (deferred)
//! * `POST /api/restart`               → `link.restart`

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::actions::{
    handle_link_config_get, handle_link_config_patch, handle_link_connect, handle_link_pair,
    handle_link_peers, handle_link_peers_remove, handle_link_qr, handle_link_register,
    handle_link_restart, handle_link_services, handle_link_status, handle_link_stun,
    handle_vpn_disable, handle_vpn_enable, handle_vpn_keygen, handle_vpn_status,
};

/// Build the axum router. Pulled out from `serve` so unit tests can
/// exercise the routes with `tower::Service` without binding a port.
///
/// `pub(crate)`, not `pub`: `axum::Router` is an HTTP-framework type and must
/// not appear in this crate's public API. The only cross-crate entrypoint is
/// [`serve`], which returns `anyhow::Result<()>` — so axum stays contained to
/// this module (see #290 axum containment).
pub(crate) fn router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/vpn/status", get(vpn_status_route))
        .route("/api/vpn/enable", post(vpn_enable_route))
        .route("/api/vpn/disable", post(vpn_disable_route))
        .route("/api/vpn/keygen", post(vpn_keygen_route))
        .route("/api/link/status", get(link_status_route))
        .route("/api/link/pair", post(link_pair_route))
        .route("/api/link/register", post(link_register_route))
        .route("/api/link/stun", get(link_stun_route))
        .route("/api/link/peers", get(link_peers_route))
        .route("/api/link/peers/remove", post(link_peers_remove_route))
        .route("/api/link/connect", post(link_connect_route))
        .route("/api/link/qr/:token", get(link_qr_route))
        .route("/api/link/config", get(link_config_get_route))
        .route("/api/link/config", patch(link_config_patch_route))
        .route("/api/link/services", get(link_services_route))
        .route("/api/restart", post(restart_route))
        .with_state(Arc::new(()))
}

/// Bind to `127.0.0.1:<port>` and run the axum server until cancelled.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("wylde-vpn: HTTP control plane listening on {addr}");
    axum::serve(
        listener,
        router().into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ── Route handlers ───────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    let status = handle_vpn_status(Value::Null).await;
    let interface_up = status
        .data
        .get("interface_up")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let connected = status
        .data
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Json(json!({
        "status": "healthy",
        "service": "wylde-vpn",
        "vpn_connected": connected,
        "interface_up": interface_up,
        "link_up": false,
        "impl": "rust-foundation-slice",
    }))
}

async fn vpn_status_route(_state: State<Arc<()>>) -> Response {
    reply_to_response(handle_vpn_status(Value::Null).await)
}

async fn vpn_enable_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_vpn_enable(unwrap_body(body)).await)
}

async fn vpn_disable_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_vpn_disable(unwrap_body(body)).await)
}

async fn vpn_keygen_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_vpn_keygen(unwrap_body(body)).await)
}

async fn link_status_route(_state: State<Arc<()>>) -> Response {
    reply_to_response(handle_link_status(Value::Null).await)
}

async fn link_pair_route(
    _state: State<Arc<()>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    // Mirror Python's `request.headers.get('X-Forwarded-For') or request.remote_addr`
    // so the per-IP pairing rate limit treats reverse-proxied clients
    // by their original IP.
    let remote_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| addr.ip().to_string());

    let mut payload = unwrap_body(body);
    if let Value::Object(ref mut obj) = payload {
        obj.entry("_remote_ip").or_insert(Value::String(remote_ip));
    } else {
        payload = json!({"_remote_ip": remote_ip});
    }
    reply_to_response(handle_link_pair(payload).await)
}

async fn link_register_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_link_register(unwrap_body(body)).await)
}

async fn link_stun_route(_state: State<Arc<()>>) -> Response {
    reply_to_response(handle_link_stun(Value::Null).await)
}

async fn link_peers_route(_state: State<Arc<()>>) -> Response {
    reply_to_response(handle_link_peers(Value::Null).await)
}

async fn link_peers_remove_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_link_peers_remove(unwrap_body(body)).await)
}

async fn link_connect_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_link_connect(unwrap_body(body)).await)
}

async fn link_qr_route(_state: State<Arc<()>>, Path(token): Path<String>) -> Response {
    let reply = handle_link_qr(json!({ "token": token })).await;
    if !reply.ok {
        return reply_to_response(reply);
    }
    // The Python service serves the SVG bytes directly with
    // `Content-Type: image/svg+xml`; mirror that.
    let svg = reply
        .data
        .get("svg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (StatusCode::OK, [("content-type", "image/svg+xml")], svg).into_response()
}

async fn link_config_get_route(_state: State<Arc<()>>) -> Response {
    reply_to_response(handle_link_config_get(Value::Null).await)
}

async fn link_services_route(_state: State<Arc<()>>) -> Response {
    reply_to_response(handle_link_services(Value::Null).await)
}

async fn link_config_patch_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_link_config_patch(unwrap_body(body)).await)
}

async fn restart_route(_state: State<Arc<()>>, body: Option<Json<Value>>) -> Response {
    reply_to_response(handle_link_restart(unwrap_body(body)).await)
}

// ── helpers ───────────────────────────────────────────────────────────────

fn unwrap_body(body: Option<Json<Value>>) -> Value {
    body.map(|Json(v)| v).unwrap_or(Value::Null)
}

/// Map an action [`Reply`] onto an axum response. Same envelope shape
/// Flask's `jsonify(...)` produces: on `ok=true`, the `data` field is
/// returned as the JSON body with HTTP 200; on `ok=false`, an
/// `{"error": "..."}` body is returned with a status code derived from
/// the error code (bad_request → 400, not_found → 404, service_unavailable → 503,
/// everything else → 500).
fn reply_to_response(reply: Reply) -> Response {
    if reply.ok {
        return (StatusCode::OK, Json(reply.data)).into_response();
    }
    let err = reply
        .error
        .unwrap_or_else(|| wylde_shared::ipc::IpcError::new("unknown", "unknown error"));
    let status = match err.code.as_str() {
        "bad_request" => StatusCode::BAD_REQUEST,
        "not_found" => StatusCode::NOT_FOUND,
        "service_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({"error": err.message, "code": err.code})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn router_builds_without_panicking() {
        let _r = router();
    }

    #[tokio::test]
    async fn reply_to_response_maps_ok_to_200() {
        let r = Reply::ok(json!({"hello": "world"}));
        let resp = reply_to_response(r);
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reply_to_response_maps_service_unavailable_to_503() {
        let r = Reply::err(wylde_shared::ipc::IpcError::new(
            "service_unavailable",
            "deferred",
        ));
        let resp = reply_to_response(r);
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn reply_to_response_maps_bad_request_to_400() {
        let r = Reply::err(wylde_shared::ipc::IpcError::new(
            "bad_request",
            "missing field",
        ));
        let resp = reply_to_response(r);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
