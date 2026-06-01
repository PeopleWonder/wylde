//! `/api/workspaces` — RAG workspace lifecycle, persona, MRU.
//!
//! Rust port of the harness surface backed by
//! `Core/harness/pipe/_rag_workspaces.py`. Python doesn't expose these
//! over HTTP — the Settings + workspace pickers talk straight to the
//! harness pipe — so this wave defines the HTTP shape. Each verb is a
//! shell around a `rag.workspaces.<verb>` pipe action.
//!
//! Note `routes::rag` also exposes `GET /api/rag/collections`, which is
//! the legacy MCP-facing alias for `rag.workspaces.list`. Both routes
//! land on the same action; the duplication mirrors Python's split
//! between the workspace-management surface and the MCP surface.
//!
//! All handlers gate on a device-gate Bearer token via
//! [`super::common::authorize`].
//!
//! ## Verb map
//!
//! | HTTP                                              | Action                            |
//! |---------------------------------------------------|-----------------------------------|
//! | `GET    /api/workspaces`                          | `rag.workspaces.list`             |
//! | `GET    /api/workspaces/recent[?limit=N]`         | `rag.workspaces.recent`           |
//! | `GET    /api/workspaces/mru_limit`                | `rag.workspaces.get_mru_limit`    |
//! | `PUT    /api/workspaces/mru_limit`                | `rag.workspaces.set_mru_limit`    |
//! | `POST   /api/workspaces/activate`                 | `rag.workspaces.activate`         |
//! | `DELETE /api/workspaces/:workspace_id`            | `rag.workspaces.delete`           |
//! | `GET    /api/workspaces/:workspace_id/status`     | `rag.workspaces.status`           |
//! | `POST   /api/workspaces/:workspace_id/reindex`    | `rag.workspaces.reindex`          |
//! | `GET    /api/workspaces/:workspace_id/persona`    | `rag.workspaces.get_persona`      |
//! | `PUT    /api/workspaces/:workspace_id/persona`    | `rag.workspaces.set_persona`      |

use std::collections::HashMap;

use axum::extract::{Json, Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Map, Value};

use super::common::{authorize, harness_dispatch};
use crate::envelopes::failure;

fn merged_payload(body: Option<Json<Value>>, extra: Vec<(&str, Value)>) -> Value {
    let mut map = match body {
        Some(Json(Value::Object(m))) => m,
        _ => Map::new(),
    };
    for (k, v) in extra {
        map.insert(k.to_owned(), v);
    }
    Value::Object(map)
}

/// `GET /api/workspaces` — list every registered workspace.
pub async fn list_workspaces(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    harness_dispatch("rag.workspaces.list", Value::Null).await
}

/// `GET /api/workspaces/recent[?limit=N]` — MRU-ordered list.
pub async fn recent_workspaces(
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = match q.get("limit").and_then(|s| s.parse::<i64>().ok()) {
        Some(n) => json!({ "limit": n }),
        None => Value::Null,
    };
    harness_dispatch("rag.workspaces.recent", payload).await
}

/// `GET /api/workspaces/mru_limit` — current MRU cap + min/max/default.
pub async fn get_mru_limit(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    harness_dispatch("rag.workspaces.get_mru_limit", Value::Null).await
}

/// `PUT /api/workspaces/mru_limit` — change the MRU cap. Body: `{"limit": N}`.
pub async fn set_mru_limit(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("rag.workspaces.set_mru_limit", payload).await
}

/// `POST /api/workspaces/activate` — open / re-open a workspace by path.
///
/// Body shape: `{"path": <path>, "full_reindex"?: bool, "conversation_id"?: str}`.
pub async fn activate_workspace(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("rag.workspaces.activate", payload).await
}

/// `DELETE /api/workspaces/:workspace_id` — drop a workspace.
pub async fn delete_workspace(headers: HeaderMap, Path(workspace_id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "rag.workspaces.delete",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

/// `GET /api/workspaces/:workspace_id/status` — read index status.
pub async fn workspace_status(headers: HeaderMap, Path(workspace_id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "rag.workspaces.status",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

/// `POST /api/workspaces/:workspace_id/reindex` — full reindex.
pub async fn reindex_workspace(headers: HeaderMap, Path(workspace_id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "rag.workspaces.reindex",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

/// `GET /api/workspaces/:workspace_id/persona` — read the persona override.
pub async fn get_persona(headers: HeaderMap, Path(workspace_id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "rag.workspaces.get_persona",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

/// `PUT /api/workspaces/:workspace_id/persona` — set persona text.
///
/// Body shape: `{"text": <persona_string>}`. An empty/missing string
/// clears the override.
pub async fn set_persona(
    headers: HeaderMap,
    Path(workspace_id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    let payload = merged_payload(body, vec![("workspace_id", Value::String(workspace_id))]);
    harness_dispatch("rag.workspaces.set_persona", payload).await
}

/// Build the workspaces sub-router.
pub fn router() -> Router {
    Router::new()
        .route("/api/workspaces", get(list_workspaces))
        .route("/api/workspaces/recent", get(recent_workspaces))
        .route("/api/workspaces/mru_limit", get(get_mru_limit))
        .route("/api/workspaces/mru_limit", put(set_mru_limit))
        .route("/api/workspaces/activate", post(activate_workspace))
        .route("/api/workspaces/:workspace_id", delete(delete_workspace))
        .route(
            "/api/workspaces/:workspace_id/status",
            get(workspace_status),
        )
        .route(
            "/api/workspaces/:workspace_id/reindex",
            post(reindex_workspace),
        )
        .route("/api/workspaces/:workspace_id/persona", get(get_persona))
        .route("/api/workspaces/:workspace_id/persona", put(set_persona))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn assert_401(method: &str, uri: &str, body: Option<&str>) {
        let app = router();
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let request = req
            .body(match body {
                Some(b) => axum::body::Body::from(b.to_owned()),
                None => axum::body::Body::empty(),
            })
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} should return 401 without a Bearer token"
        );
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "missing_token");
    }

    #[tokio::test]
    async fn list_without_token_returns_401() {
        assert_401("GET", "/api/workspaces", None).await;
    }

    #[tokio::test]
    async fn recent_without_token_returns_401() {
        assert_401("GET", "/api/workspaces/recent?limit=5", None).await;
    }

    #[tokio::test]
    async fn get_mru_limit_without_token_returns_401() {
        assert_401("GET", "/api/workspaces/mru_limit", None).await;
    }

    #[tokio::test]
    async fn set_mru_limit_without_token_returns_401() {
        assert_401("PUT", "/api/workspaces/mru_limit", Some(r#"{"limit":10}"#)).await;
    }

    #[tokio::test]
    async fn activate_without_token_returns_401() {
        assert_401(
            "POST",
            "/api/workspaces/activate",
            Some(r#"{"path":"C:/x"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn delete_without_token_returns_401() {
        assert_401("DELETE", "/api/workspaces/ws1", None).await;
    }

    #[tokio::test]
    async fn status_without_token_returns_401() {
        assert_401("GET", "/api/workspaces/ws1/status", None).await;
    }

    #[tokio::test]
    async fn reindex_without_token_returns_401() {
        assert_401("POST", "/api/workspaces/ws1/reindex", None).await;
    }

    #[tokio::test]
    async fn get_persona_without_token_returns_401() {
        assert_401("GET", "/api/workspaces/ws1/persona", None).await;
    }

    #[tokio::test]
    async fn set_persona_without_token_returns_401() {
        assert_401(
            "PUT",
            "/api/workspaces/ws1/persona",
            Some(r#"{"text":"You are…"}"#),
        )
        .await;
    }
}
