//! `/api/conversations` — chat-history CRUD.
//!
//! Rust port of the conversation-history surface the Python GUI reaches
//! via `Core/harness/pipe/_conversations.py`. Python doesn't expose
//! these over HTTP yet — the GUI talks straight to the harness pipe —
//! so this wave defines the HTTP shape: every verb is a thin shell
//! around the `conversations.*` pipe action, with `proxy_core::pipe_action`
//! providing the (status, envelope) translation when the harness pipe
//! is unreachable or returns a structured error.
//!
//! All handlers gate on a device-gate Bearer token via
//! [`super::common::authorize`] to keep the surface consistent with the
//! mobile-bound `/api/chat/run_turn` route — chat history is just as
//! sensitive as the chat content itself.
//!
//! Wire shape: HTTP 200 carries `{ok: true, data: <action reply>}` where
//! `<action reply>` is the unmodified return value of the harness pipe
//! handler (matches the GUI client's expectations byte-for-byte).

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};

use super::common::{authorize, harness_dispatch};
use crate::envelopes::failure;

/// `GET /api/conversations` — list every saved conversation, newest-first.
pub async fn list_conversations(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    harness_dispatch("conversations.list", Value::Null).await
}

/// `POST /api/conversations` — mint a fresh conversation id.
pub async fn new_conversation(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    harness_dispatch("conversations.new", Value::Null).await
}

/// `GET /api/conversations/{id}` — read one conversation by id.
pub async fn get_conversation(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if id.trim().is_empty() {
        return failure("bad_request", "id is required", StatusCode::BAD_REQUEST);
    }
    harness_dispatch("conversations.get", json!({ "id": id })).await
}

/// `DELETE /api/conversations/{id}` — drop one conversation by id.
pub async fn delete_conversation(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if id.trim().is_empty() {
        return failure("bad_request", "id is required", StatusCode::BAD_REQUEST);
    }
    harness_dispatch("conversations.delete", json!({ "id": id })).await
}

/// Build the conversations sub-router.
pub fn router() -> Router {
    Router::new()
        .route("/api/conversations", get(list_conversations))
        .route("/api/conversations", post(new_conversation))
        .route("/api/conversations/{id}", get(get_conversation))
        .route("/api/conversations/{id}", delete(delete_conversation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn list_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/conversations")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "missing_token");
    }

    #[tokio::test]
    async fn delete_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/conversations/abc")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn new_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/conversations")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn get_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/conversations/abc")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
