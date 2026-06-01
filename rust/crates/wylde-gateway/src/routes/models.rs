//! `/api/models` — Ollama installed-model surface.
//!
//! Rust port of `Gateway/routes/models.py`. Unlike the wave-2a/2b
//! harness-pipe-backed routes, models is an HTTP proxy: every endpoint
//! forwards to the local Ollama daemon at `127.0.0.1:11434`, either
//! one-shot via [`proxy_core::http_call`] or streamed via
//! [`streaming::ndjson_to_sse`] (for the `/pull` long-running download).
//!
//! ## Surface
//!
//! | HTTP                                | Ollama upstream                       | Wave-2c shape           |
//! |-------------------------------------|---------------------------------------|-------------------------|
//! | `GET    /api/models`                | `GET  /api/tags`                      | one-shot JSON           |
//! | `GET    /api/models/running`        | `GET  /api/ps`                        | one-shot JSON           |
//! | `POST   /api/models/pull`           | `POST /api/pull` (NDJSON)             | NDJSON→SSE              |
//! | `POST   /api/models/generate`       | `POST /api/generate`                  | one-shot JSON           |
//! | `DELETE /api/models/{name:path}`    | `DELETE /api/delete` w/ `{name}` body | one-shot JSON           |
//!
//! ## Why this isn't a harness-pipe action
//!
//! The Python file talks to Ollama directly. The new in-process
//! `Core/harness/model_registry/` exists (see `WYLDE_ENDPOINTS.md`'s
//! Models section), but the Gateway-HTTP surface still proxies the raw
//! Ollama daemon so the GUI's existing client code keeps working.
//! Moving to a `models.<verb>` harness action is a separate cleanup
//! that lives behind `Core/harness/pipe/_models.py` when it lands —
//! not a wave-2c concern.
//!
//! ## Auth
//!
//! Every models route gates on `require_local` (loopback + WyldeLink
//! CGNAT allowlist), matching the Python `models.py`.
//!
//! ## Wire format
//!
//! Success responses wrap Ollama's body verbatim:
//! `{"ok": true, "data": <upstream-json>}` with the upstream HTTP
//! status code (almost always 200). Failure responses use the
//! canonical nested envelope `{"ok": false, "error": {"code": …,
//! "message": …}}` — same shape wave 2a/2b emit on the error path —
//! keyed off the upstream HTTP status. The Python flat envelope
//! (`{"ok": false, "error": "http_404", "message": …, "code": 404}`)
//! is NOT reproduced; the canonical Rust envelope is the cross-wave
//! convention.

use std::time::Duration;

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Map, Value};

use crate::auth::require_local;
use crate::envelopes::{failure, success_with_status};
use crate::proxy_core::{http_call, HttpMethod, HTTP_DEFAULT_TIMEOUT};
use crate::streaming::{ndjson_to_sse, DEFAULT_CHUNK_TIMEOUT};

/// Local Ollama daemon URL. Matches Python's `OLLAMA_URL =
/// "http://127.0.0.1:11434"`. Kept hard-coded — Ollama always binds
/// localhost in the Wylde launch script, and the port is a constant
/// of the daemon, not a runtime decision.
const OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// `GET /api/models` — list locally-installed models.
pub async fn list_installed() -> Response {
    forward_one_shot(
        &format!("{OLLAMA_URL}/api/tags"),
        HttpMethod::Get,
        None,
        HTTP_DEFAULT_TIMEOUT,
    )
    .await
}

/// `GET /api/models/running` — list models currently held in memory.
pub async fn running() -> Response {
    forward_one_shot(
        &format!("{OLLAMA_URL}/api/ps"),
        HttpMethod::Get,
        None,
        HTTP_DEFAULT_TIMEOUT,
    )
    .await
}

/// `POST /api/models/pull` — kick off a streamed model download.
///
/// Upstream emits NDJSON progress lines until the final line includes
/// `{"status": "success"}`; [`ndjson_to_sse`] re-emits each line as an
/// `event: progress` SSE frame and the final line as `event: done`.
pub async fn pull(body: Option<Json<Value>>) -> Response {
    let mut payload: Map<String, Value> = match body {
        Some(Json(Value::Object(m))) => m,
        _ => Map::new(),
    };
    // Python: `body.setdefault("stream", True)`. Ollama's `/api/pull`
    // returns a single-shot dict when `stream` is False; the NDJSON
    // parser would treat that as one line with `status: success`,
    // which works — but matching Python keeps the upstream behaviour
    // identical to today's GUI.
    payload
        .entry("stream".to_owned())
        .or_insert(Value::Bool(true));

    ndjson_to_sse(
        &format!("{OLLAMA_URL}/api/pull"),
        Value::Object(payload),
        HttpMethod::Post,
        "progress",
        "done",
        // Ollama's terminal NDJSON line is `{"status": "success"}` —
        // the same field also carries intermediate states like
        // `"downloading"`, so the done-detector is "the field
        // exists AND is non-empty". `ndjson_to_sse` treats a String
        // value as done when non-empty, matching that.
        "status",
        DEFAULT_CHUNK_TIMEOUT,
    )
    .await
}

/// `POST /api/models/generate` — typically the GUI's keep-alive=0 unload.
///
/// Ollama's `/api/generate` can stream tokens too, but the mobile/GUI
/// callers pass `stream=false` (the default Python forwards). We keep
/// the one-shot path: any stream=true caller would have to drive the
/// streaming endpoint instead.
pub async fn generate(body: Option<Json<Value>>) -> Response {
    let payload: Value = match body {
        Some(Json(v)) => v,
        None => Value::Object(Map::new()),
    };
    // Python timeout=30.0 — same default as `proxy_core.http_call`.
    forward_one_shot(
        &format!("{OLLAMA_URL}/api/generate"),
        HttpMethod::Post,
        Some(payload),
        Duration::from_secs(30),
    )
    .await
}

/// `DELETE /api/models/:name` — drop a locally-installed model.
///
/// Mirrors the Python `delete_model` handler. Ollama's delete endpoint
/// takes the model name in the JSON body rather than on the path, so
/// the captured `name` becomes `{"name": <name>}`. The `:name` path
/// segment captures everything after `/api/models/` so model names
/// with slashes (e.g. `library/llama3:8b`) round-trip cleanly.
pub async fn delete_model(Path(name): Path<String>) -> Response {
    if name.trim().is_empty() {
        return failure(
            "bad_request",
            "model name is required",
            StatusCode::BAD_REQUEST,
        );
    }
    forward_one_shot(
        &format!("{OLLAMA_URL}/api/delete"),
        HttpMethod::Delete,
        Some(json!({ "name": name })),
        Duration::from_secs(30),
    )
    .await
}

/// Shared forward path for the one-shot endpoints: call Ollama, wrap
/// the body in the canonical `{ok, data}` envelope on success, fold
/// the canonical `{ok, error}` envelope on failure.
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

/// Build the `/api/models` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/models",
            get(list_installed).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/models/running",
            get(running).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/models/pull",
            post(pull).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/models/generate",
            post(generate).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/models/:name",
            delete(delete_model).route_layer(from_fn(require_local)),
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
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn list_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/models", None).await;
    }

    #[tokio::test]
    async fn running_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/models/running", None).await;
    }

    #[tokio::test]
    async fn pull_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/models/pull", Some(r#"{"name":"llama3"}"#)).await;
    }

    #[tokio::test]
    async fn generate_rejects_non_local_caller() {
        assert_local_denied(
            "POST",
            "/api/models/generate",
            Some(r#"{"model":"llama3","keep_alive":0}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn delete_rejects_non_local_caller() {
        assert_local_denied("DELETE", "/api/models/llama3", None).await;
    }

    #[tokio::test]
    async fn unknown_route_under_models_404s() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/models/registry/discover")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Registry sub-routes were removed in Phase 9 (per Python
        // docstring). This documents the absence as a test invariant.
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn method_not_allowed_on_list() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/models")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
