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
//! | `DELETE /api/workspaces/{workspace_id}`            | `rag.workspaces.delete`           |
//! | `GET    /api/workspaces/{workspace_id}/status`     | `rag.workspaces.status`           |
//! | `POST   /api/workspaces/{workspace_id}/reindex`    | `rag.workspaces.reindex`          |
//! | `GET    /api/workspaces/{workspace_id}/persona`    | `rag.workspaces.get_persona`      |
//! | `PUT    /api/workspaces/{workspace_id}/persona`    | `rag.workspaces.set_persona`      |

use std::collections::HashMap;

use axum::extract::{Json, Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Map, Value};

use super::common::{authorize, workspaces_dispatch};
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

/// Endpoints whose backing verb was retired by the config-file-backed
/// workspaces redesign (the LanceDB file indexer + the configurable MRU
/// cap are gone). Returns 410 so a stale HTTP consumer gets a clear
/// signal rather than a `no_action` 500.
fn retired(what: &str) -> Response {
    failure(
        "retired",
        &format!("{what} was retired in the workspaces redesign"),
        StatusCode::GONE,
    )
}

/// `GET /api/workspaces` — list the MRU-5 workspaces.
pub async fn list_workspaces(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    workspaces_dispatch("workspaces.list_mru", Value::Null).await
}

/// `GET /api/workspaces/recent[?limit=N]` — MRU-5 list (limit ignored;
/// the window is the harness's static MRU-5).
pub async fn recent_workspaces(
    headers: HeaderMap,
    Query(_q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    workspaces_dispatch("workspaces.list_mru", Value::Null).await
}

/// `GET /api/workspaces/mru_limit` — retired (static MRU-5).
pub async fn get_mru_limit(headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    retired("the configurable MRU limit")
}

/// `PUT /api/workspaces/mru_limit` — retired (static MRU-5).
pub async fn set_mru_limit(headers: HeaderMap, _body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    retired("the configurable MRU limit")
}

/// `POST /api/workspaces/activate` — register + activate a workspace by
/// path. Redesign replacement for `rag.workspaces.activate`. Body:
/// `{"path": <path>, ...}` — `path` is remapped to the redesign's
/// `folder` field; `full_reindex` / `conversation_id` are ignored.
pub async fn activate_workspace(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let folder = body
        .and_then(|Json(v)| {
            v.get("path")
                .or_else(|| v.get("folder"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if folder.trim().is_empty() {
        return failure("bad_request", "path is required", StatusCode::BAD_REQUEST);
    }
    workspaces_dispatch("workspaces.create", json!({ "folder": folder })).await
}

/// `DELETE /api/workspaces/{workspace_id}` — drop a workspace.
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
    workspaces_dispatch("workspaces.delete", json!({ "workspace_id": workspace_id })).await
}

/// `GET /api/workspaces/{workspace_id}/status` — retired (no file indexer).
pub async fn workspace_status(headers: HeaderMap, _workspace_id: Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    retired("workspace index status")
}

/// `POST /api/workspaces/{workspace_id}/reindex` — retired (no file indexer).
pub async fn reindex_workspace(headers: HeaderMap, _workspace_id: Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    retired("workspace re-index")
}

/// `GET /api/workspaces/{workspace_id}/persona` — retired (no read verb;
/// persona now lives in `persona.md`, read via the workspace bundle).
pub async fn get_persona(headers: HeaderMap, _workspace_id: Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    retired("the persona read endpoint")
}

/// `PUT /api/workspaces/{workspace_id}/persona` — set persona text.
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
    workspaces_dispatch("workspaces.set_persona", payload).await
}

/// Build the workspaces sub-router.
pub fn router() -> Router {
    Router::new()
        .route("/api/workspaces", get(list_workspaces))
        .route("/api/workspaces/recent", get(recent_workspaces))
        .route("/api/workspaces/mru_limit", get(get_mru_limit))
        .route("/api/workspaces/mru_limit", put(set_mru_limit))
        .route("/api/workspaces/activate", post(activate_workspace))
        .route("/api/workspaces/{workspace_id}", delete(delete_workspace))
        .route(
            "/api/workspaces/{workspace_id}/status",
            get(workspace_status),
        )
        .route(
            "/api/workspaces/{workspace_id}/reindex",
            post(reindex_workspace),
        )
        .route("/api/workspaces/{workspace_id}/persona", get(get_persona))
        .route("/api/workspaces/{workspace_id}/persona", put(set_persona))
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
