//! `/api/rag` — retrieval-augmented generation proxy.
//!
//! Rust port of `Gateway/routes/rag.py`. The Python router is the
//! intentional MCP surface — Claude Desktop and other external agents
//! query the Wylde user's indexed workspaces here without speaking the harness
//! pipe directly. Three endpoints, each a shell around a harness action:
//!
//! | HTTP                          | Action                                              |
//! |-------------------------------|-----------------------------------------------------|
//! | `POST /api/rag/query`         | `tools.run` with `name=rag_ask`, `args=<body>`      |
//! | `POST /api/rag/ingest`        | `tools.run` with `name=rag_index`, `args=<body>`    |
//! | `GET  /api/rag/collections`   | `rag.workspaces.list`                               |
//!
//! Every rag route gates on `require_local` (the WyldeLink CGNAT +
//! loopback CIDR tier), matching the Python `rag.py`.
//!
//! No SSE in this wave: `rag_ask` returns its result synchronously
//! through the pipe (`tools.run` is a one-shot dispatch, not a stream).

use axum::extract::Json;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use super::common::harness_dispatch;
use crate::auth::require_local;

/// `POST /api/rag/query` — semantic search via the `rag_ask` tool.
pub async fn query(body: Option<Json<Value>>) -> Response {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("tools.run", json!({ "name": "rag_ask", "args": args })).await
}

/// `POST /api/rag/ingest` — incremental indexing via the `rag_index` tool.
pub async fn ingest(body: Option<Json<Value>>) -> Response {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("tools.run", json!({ "name": "rag_index", "args": args })).await
}

/// `GET /api/rag/collections` — list configured workspaces. Equivalent
/// to `GET /api/workspaces` in [`super::workspaces`]; kept here as the
/// legacy MCP-facing alias.
pub async fn collections() -> Response {
    harness_dispatch("workspaces.list_mru", Value::Null).await
}

/// Build the rag sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/rag/query",
            post(query).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/rag/ingest",
            post(ingest).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/rag/collections",
            get(collections).route_layer(from_fn(require_local)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

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
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn query_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/rag/query", Some(r#"{"q":"x"}"#)).await;
    }

    #[tokio::test]
    async fn ingest_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/rag/ingest",
            Some(r#"{"path":"C:/x","workspace_id":"ws1"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn collections_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/rag/collections", None).await;
    }
}
