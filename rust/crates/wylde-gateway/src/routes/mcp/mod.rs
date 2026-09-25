//! MCP server surface — `/mcp` Streamable HTTP endpoint.
//!
//! Rust port of `Gateway/routes/mcp/`. Wylde exposes a v1 Model Context
//! Protocol server so external clients (Claude Desktop, the Anthropic
//! API, Cursor, …) can reach the harness's tool / resource / prompt
//! catalogs through one standard protocol instead of speaking the Wylde
//! pipe themselves.
//!
//! The module splits three ways, mirroring the Python package:
//!
//! * [`transport`] — Streamable HTTP framing + session management.
//! * [`handlers`]  — JSON-RPC method dispatch.
//! * [`adapters`]  — bridges between MCP shapes and harness pipe actions.
//!
//! One endpoint handles both verbs: `POST /mcp` carries client → server
//! JSON-RPC; `GET /mcp` would open the server → client SSE stream,
//! which v1 does not use (it returns `405`).
//!
//! Auth: `require_device` — the same device-gate Bearer-token tier as
//! `POST /api/chat/run_turn`. MCP clients authenticate with a device
//! token.
//!
//! MCP spec: <https://spec.modelcontextprotocol.io/> (revision 2025-06-18).
//! See `docs/mcp_surface.md` for the exposed surface.

mod adapters;
mod authz;
mod handlers;
mod resources;
mod transport;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::from_fn;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Router};

use crate::auth::{require_device, Device};

/// `POST /mcp` — client → server JSON-RPC over the Streamable HTTP
/// transport.
///
/// `require_device` runs first (route layer) and inserts the verified
/// [`Device`]; we read it here so the dispatcher can authorize per caller
/// rather than trusting any authenticated device with the full surface.
async fn mcp_post(
    Extension(device): Extension<Device>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session_id = headers
        .get(transport::SESSION_HEADER)
        .and_then(|v| v.to_str().ok());
    let outcome = transport::process_post(&device, &body, session_id).await;

    let mut builder = Response::builder().status(outcome.status);
    if let Some(sid) = &outcome.new_session {
        builder = builder.header("Mcp-Session-Id", sid.as_str());
    }
    match outcome.body {
        Some(value) => {
            let bytes = serde_json::to_vec(&value).unwrap_or_default();
            builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        // Notification — JSON-RPC mandates no response payload.
        None => builder
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

/// `GET /mcp` — server → client SSE stream. v1 emits nothing, so per the
/// spec this returns `405 Method Not Allowed`.
async fn mcp_get() -> Response {
    let body = serde_json::to_vec(&transport::unsupported_get()).unwrap_or_default();
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ALLOW, "POST")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Build the MCP sub-router. Both verbs on `/mcp` gate on `require_device`.
pub fn router() -> Router {
    Router::new()
        .route("/mcp", post(mcp_post).get(mcp_get))
        .route_layer(from_fn(require_device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token_cache::{global as token_cache, Device};
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    fn post_request(token: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }

    #[tokio::test]
    async fn post_without_token_is_401() {
        let resp = router()
            .oneshot(post_request(
                None,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "missing_token");
    }

    #[tokio::test]
    async fn get_without_token_is_401() {
        let resp = router()
            .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn get_with_token_is_405_not_allowed() {
        let token = "mcp-mod-test-token-get";
        token_cache()
            .insert(
                token.to_owned(),
                Device {
                    device_id: "dev-mcp-get".to_owned(),
                    tier: "tool_use".to_owned(),
                },
            )
            .await;
        let resp = router()
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(resp.headers().get(header::ALLOW).unwrap(), "POST");
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], handlers::INVALID_REQUEST);
    }

    /// Full HTTP path: a real JSON-RPC `initialize` request through the
    /// router, with `require_device` satisfied from the token cache.
    /// `initialize` touches no pipe, so this verifies the end-to-end
    /// transport + handshake without a live harness.
    #[tokio::test]
    async fn post_initialize_round_trips_through_the_router() {
        let token = "mcp-mod-test-token-init";
        token_cache()
            .insert(
                token.to_owned(),
                Device {
                    device_id: "dev-mcp-init".to_owned(),
                    tier: "tool_use".to_owned(),
                },
            )
            .await;
        let resp = router()
            .oneshot(post_request(
                Some(token),
                r#"{"jsonrpc":"2.0","id":42,"method":"initialize"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get("mcp-session-id").is_some(),
            "initialize must mint a session id header"
        );
        let body = to_bytes(resp.into_body(), 16 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(
            v["result"]["protocolVersion"],
            handlers::MCP_PROTOCOL_VERSION
        );
        assert_eq!(v["result"]["serverInfo"]["name"], handlers::SERVER_NAME);
    }

    #[tokio::test]
    async fn post_notification_round_trips_as_202() {
        let token = "mcp-mod-test-token-notif";
        token_cache()
            .insert(
                token.to_owned(),
                Device {
                    device_id: "dev-mcp-notif".to_owned(),
                    tier: "tool_use".to_owned(),
                },
            )
            .await;
        let resp = router()
            .oneshot(post_request(
                Some(token),
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(body.is_empty(), "a notification gets no response body");
    }

    /// Seed a device token of the given tier and POST a `tools/call` for
    /// `tool` through the full router; return the parsed JSON-RPC body.
    async fn tools_call_body(token: &str, tier: &str, tool: &str) -> Value {
        token_cache()
            .insert(
                token.to_owned(),
                Device {
                    device_id: format!("dev-{token}"),
                    tier: tier.to_owned(),
                },
            )
            .await;
        let req_body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{{}}}}}}"#
        );
        let resp = router()
            .oneshot(post_request(Some(token), &req_body))
            .await
            .unwrap();
        // A JSON-RPC application error rides a 200.
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn tools_call_refuses_a_non_allowlisted_tool_over_the_router() {
        // `execute_bash` is a real harness tool but not MCP-exposable.
        let v = tools_call_body("mcp-auth-nonlisted", "tool_use", "execute_bash").await;
        assert_eq!(v["error"]["code"], handlers::TOOL_NOT_PERMITTED);
    }

    #[tokio::test]
    async fn tools_call_refuses_a_destructive_tool_over_the_router() {
        // Destructive tools are never exposed — refused before the pipe,
        // even though this device authenticated fine.
        let v = tools_call_body("mcp-auth-destructive", "tool_use", "write_file").await;
        assert_eq!(v["error"]["code"], handlers::TOOL_NOT_PERMITTED);
    }

    #[tokio::test]
    async fn tools_call_tier_wins_over_confirm_via_router() {
        // A non-privileged (tool_use) device cannot run an off-list
        // destructive tool even with confirm:true — refused pre-pipe, so
        // the assertion is deterministic without a harness. (The
        // destructive_tool_access + confirm path needs a live catalog and
        // is covered by handlers::decide_matrix instead.)
        let token = "mcp-auth-confirm-tierwins";
        token_cache()
            .insert(
                token.to_owned(),
                Device {
                    device_id: "dev-confirm".to_owned(),
                    tier: "tool_use".to_owned(),
                },
            )
            .await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"write_file","arguments":{"confirm":true,"path":"/x"}}}"#;
        let resp = router()
            .oneshot(post_request(Some(token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], handlers::TOOL_NOT_PERMITTED);
    }

    #[tokio::test]
    async fn tools_call_allows_an_allowlisted_tool_past_authorization() {
        // An allow-listed tool clears the authorization gate and reaches
        // the bridge; with no harness pipe in the test sandbox that
        // surfaces as INTERNAL_ERROR — crucially NOT the TOOL_NOT_PERMITTED
        // authorization refusal.
        let v = tools_call_body("mcp-auth-allowed", "tool_use", "read_file").await;
        assert_eq!(v["error"]["code"], handlers::INTERNAL_ERROR);
    }
}
