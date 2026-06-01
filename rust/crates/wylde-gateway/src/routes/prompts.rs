//! `/api/prompts` — system-prompt overrides + preset CRUD.
//!
//! Rust port of the surface backed by `Core/harness/pipe/_prompts.py`.
//! Python doesn't expose this over HTTP — the Settings page reaches the
//! harness pipe directly — so this wave defines the HTTP shape. Each
//! verb dispatches to a `prompts.*` pipe action and returns the harness
//! reply wrapped in the standard `{ok: true, data: …}` envelope.
//!
//! Authentication mirrors `routes::conversations`: every handler requires
//! a device-gate Bearer token. The prompt registry shapes how the agent
//! talks, so it lives behind the same auth boundary as chat history.
//!
//! ## Verb map
//!
//! | HTTP                                      | Action                  |
//! |-------------------------------------------|-------------------------|
//! | `GET    /api/prompts`                     | `prompts.list`          |
//! | `POST   /api/prompts`                     | `prompts.save`          |
//! | `POST   /api/prompts/presets`             | `prompts.save_preset`   |
//! | `PUT    /api/prompts/active`              | `prompts.set_active`    |
//! | `DELETE /api/prompts/presets/:name`       | `prompts.delete_preset` |
//!
//! Request bodies are forwarded as the action payload verbatim — the
//! harness handlers do their own shape validation and emit
//! `[bad_request]`-tagged errors that `proxy_core::pipe_action` then
//! translates to HTTP 400.

use axum::extract::{Json, Path};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Value};

use super::common::{authorize, harness_dispatch};
use crate::envelopes::failure;

/// `GET /api/prompts` — return groups, catalog, overrides, presets,
/// active_preset in one round-trip (matches the Settings page hydration).
pub async fn list_prompts(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    harness_dispatch("prompts.list", Value::Null).await
}

/// `POST /api/prompts` — save an override for one prompt id, or clear it
/// by sending `text: null` / a string matching the catalog default.
///
/// Body shape: `{"id": <prompt_id>, "text": <string|null>}`.
pub async fn save_prompt(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("prompts.save", payload).await
}

/// `POST /api/prompts/presets` — snapshot the current overrides into a
/// named preset and activate it.
///
/// Body shape: `{"name": <preset_name>}`.
pub async fn save_preset(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("prompts.save_preset", payload).await
}

/// `PUT /api/prompts/active` — activate the named preset (or reset to
/// catalog defaults for `"Default"`).
///
/// Body shape: `{"name": <preset_name>}`.
pub async fn set_active_preset(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("prompts.set_active", payload).await
}

/// `DELETE /api/prompts/presets/:name` — drop a saved preset.
pub async fn delete_preset(headers: HeaderMap, Path(name): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if name.trim().is_empty() {
        return failure("bad_request", "name is required", StatusCode::BAD_REQUEST);
    }
    harness_dispatch("prompts.delete_preset", json!({ "name": name })).await
}

/// Build the prompts sub-router.
pub fn router() -> Router {
    Router::new()
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts", post(save_prompt))
        .route("/api/prompts/presets", post(save_preset))
        .route("/api/prompts/active", put(set_active_preset))
        .route("/api/prompts/presets/:name", delete(delete_preset))
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
                    .uri("/api/prompts")
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
    async fn save_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/prompts")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"id":"x","text":"y"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn save_preset_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/prompts/presets")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"name":"My Preset"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn set_active_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/prompts/active")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"name":"Default"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn delete_preset_without_token_returns_401() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/prompts/presets/foo")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
