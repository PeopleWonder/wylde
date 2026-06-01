//! `/api/memory` — memory CRUD across the three layers.
//!
//! Rust port of the harness-pipe surface backed by
//! `Core/harness/pipe/_memory.py`. Python doesn't expose memory over HTTP
//! today — the GUI / scheduler talk straight to the harness pipe — so
//! this wave defines the HTTP shape. Every verb is a thin shell around a
//! `memory.<layer>.<verb>` pipe action, with `proxy_core::pipe_action`
//! handling the (status, envelope) translation when the harness pipe is
//! unreachable or returns a structured `[bad_request]` / `[not_found]`
//! error.
//!
//! All handlers gate on a device-gate Bearer token via
//! [`super::common::authorize`] — long-term and workspace memory are
//! as sensitive as the chat history, and short-term memory carries the
//! in-flight working set for the conversation surface.
//!
//! ## Verb map
//!
//! | HTTP                                                          | Action                          |
//! |---------------------------------------------------------------|---------------------------------|
//! | `GET    /api/memory/long_term`                                | `memory.long_term.list`         |
//! | `POST   /api/memory/long_term/search`                         | `memory.long_term.search`       |
//! | `POST   /api/memory/long_term`                                | `memory.long_term.save`         |
//! | `PUT    /api/memory/long_term/:id`                            | `memory.long_term.update`       |
//! | `DELETE /api/memory/long_term/:id`                            | `memory.long_term.delete`       |
//! | `GET    /api/memory/long_term/:id/history`                    | `memory.long_term.history`      |
//! | `GET    /api/memory/workspace/:workspace_id`                  | `memory.workspace.list`         |
//! | `POST   /api/memory/workspace/:workspace_id/search`           | `memory.workspace.search`       |
//! | `POST   /api/memory/workspace/:workspace_id`                  | `memory.workspace.save`         |
//! | `PUT    /api/memory/workspace/:workspace_id/:id`              | `memory.workspace.update`       |
//! | `DELETE /api/memory/workspace/:workspace_id/:id`              | `memory.workspace.delete`       |
//! | `POST   /api/memory/workspace/:workspace_id/curate`           | `memory.workspace.curate`       |
//! | `GET    /api/memory/short_term/:conversation_id`              | `memory.short_term.get`         |
//! | `POST   /api/memory/short_term/:conversation_id`              | `memory.short_term.append`      |
//! | `DELETE /api/memory/short_term/:conversation_id`              | `memory.short_term.clear`       |
//! | `POST   /api/memory/reflect`                                  | `memory.reflect`                |

use std::collections::HashMap;

use axum::extract::{Json, Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Map, Value};

use super::common::{authorize, harness_dispatch};
use crate::envelopes::failure;

/// Merge `extra` into the request body (if any) and dispatch. Path-derived
/// keys win over body keys with the same name so a client can't override
/// an id locked in by the URL.
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

/// Convert a `HashMap<String, String>` query map to a JSON object payload.
/// Bool-shaped values (`"true"`/`"false"`) are parsed; everything else
/// stays as a string — the harness action does its own typing/validation.
fn query_payload(q: HashMap<String, String>) -> Value {
    let mut map = Map::new();
    for (k, v) in q {
        let parsed = match v.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(v),
        };
        map.insert(k, parsed);
    }
    Value::Object(map)
}

// ── Memory: long-term ──────────────────────────────────────────────────

/// `GET /api/memory/long_term[?include_superseded=true]` — list records.
pub async fn long_term_list(
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = if q.is_empty() {
        Value::Null
    } else {
        query_payload(q)
    };
    harness_dispatch("memory.long_term.list", payload).await
}

/// `POST /api/memory/long_term/search` — semantic search.
pub async fn long_term_search(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("memory.long_term.search", payload).await
}

/// `POST /api/memory/long_term` — save a new long-term record.
pub async fn long_term_save(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("memory.long_term.save", payload).await
}

/// `PUT /api/memory/long_term/:id` — update fields on an existing record.
pub async fn long_term_update(
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if id.trim().is_empty() {
        return failure("bad_request", "id is required", StatusCode::BAD_REQUEST);
    }
    let payload = merged_payload(body, vec![("id", Value::String(id))]);
    harness_dispatch("memory.long_term.update", payload).await
}

/// `DELETE /api/memory/long_term/:id` — drop one long-term record.
pub async fn long_term_delete(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if id.trim().is_empty() {
        return failure("bad_request", "id is required", StatusCode::BAD_REQUEST);
    }
    harness_dispatch("memory.long_term.delete", json!({ "id": id })).await
}

/// `GET /api/memory/long_term/:id/history` — show the supersession chain.
pub async fn long_term_history(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if id.trim().is_empty() {
        return failure("bad_request", "id is required", StatusCode::BAD_REQUEST);
    }
    harness_dispatch("memory.long_term.history", json!({ "id": id })).await
}

// ── Memory: workspace ─────────────────────────────────────────────────
//
// NOTE: these verbs are `memory.workspace.*` (SINGULAR) on purpose —
// they are the workspace-scoped *memory CRUD* surface (list / search /
// save / update / delete / curate of records inside one workspace),
// served by Python `Core/harness/pipe/_memory.py`.  Do NOT "correct"
// them to the plural `memory.workspaces.*`: that is the wholly separate
// workspace *registry* surface (list / recent / get / persona / MRU)
// served by the Rust harness, and it has no search/save/update/curate
// verbs.  The two namespaces coexist by design.

/// `GET /api/memory/workspace/:workspace_id[?include_superseded=true]`.
pub async fn workspace_list(
    headers: HeaderMap,
    Path(workspace_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
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
    let mut payload = match query_payload(q) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    payload.insert("workspace_id".into(), Value::String(workspace_id));
    harness_dispatch("memory.workspace.list", Value::Object(payload)).await
}

/// `POST /api/memory/workspace/:workspace_id/search`.
pub async fn workspace_search(
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
    harness_dispatch("memory.workspace.search", payload).await
}

/// `POST /api/memory/workspace/:workspace_id` — save a workspace memory.
pub async fn workspace_save(
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
    harness_dispatch("memory.workspace.save", payload).await
}

/// `PUT /api/memory/workspace/:workspace_id/:id` — update workspace memory.
pub async fn workspace_update(
    headers: HeaderMap,
    Path((workspace_id, id)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() || id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id and id are required",
            StatusCode::BAD_REQUEST,
        );
    }
    let payload = merged_payload(
        body,
        vec![
            ("workspace_id", Value::String(workspace_id)),
            ("id", Value::String(id)),
        ],
    );
    harness_dispatch("memory.workspace.update", payload).await
}

/// `DELETE /api/memory/workspace/:workspace_id/:id`.
pub async fn workspace_delete(
    headers: HeaderMap,
    Path((workspace_id, id)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if workspace_id.trim().is_empty() || id.trim().is_empty() {
        return failure(
            "bad_request",
            "workspace_id and id are required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "memory.workspace.delete",
        json!({ "workspace_id": workspace_id, "id": id }),
    )
    .await
}

/// `POST /api/memory/workspace/:workspace_id/curate` — trigger curation.
pub async fn workspace_curate(headers: HeaderMap, Path(workspace_id): Path<String>) -> Response {
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
        "memory.workspace.curate",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

// ── Memory: short-term ────────────────────────────────────────────────

/// `GET /api/memory/short_term/:conversation_id` — read working memory.
pub async fn short_term_get(headers: HeaderMap, Path(conversation_id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if conversation_id.trim().is_empty() {
        return failure(
            "bad_request",
            "conversation_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "memory.short_term.get",
        json!({ "conversation_id": conversation_id }),
    )
    .await
}

/// `POST /api/memory/short_term/:conversation_id` — append an entry.
pub async fn short_term_append(
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if conversation_id.trim().is_empty() {
        return failure(
            "bad_request",
            "conversation_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    let payload = merged_payload(
        body,
        vec![("conversation_id", Value::String(conversation_id))],
    );
    harness_dispatch("memory.short_term.append", payload).await
}

/// `DELETE /api/memory/short_term/:conversation_id` — clear working memory.
pub async fn short_term_clear(headers: HeaderMap, Path(conversation_id): Path<String>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    if conversation_id.trim().is_empty() {
        return failure(
            "bad_request",
            "conversation_id is required",
            StatusCode::BAD_REQUEST,
        );
    }
    harness_dispatch(
        "memory.short_term.clear",
        json!({ "conversation_id": conversation_id }),
    )
    .await
}

// ── Memory: reflection ────────────────────────────────────────────────

/// `POST /api/memory/reflect` — run a consolidation cycle.
pub async fn reflect(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Err(resp) = authorize(&headers).await {
        return resp;
    }
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    harness_dispatch("memory.reflect", payload).await
}

/// Build the memory sub-router.
pub fn router() -> Router {
    Router::new()
        // long-term
        .route("/api/memory/long_term", get(long_term_list))
        .route("/api/memory/long_term", post(long_term_save))
        .route("/api/memory/long_term/search", post(long_term_search))
        .route("/api/memory/long_term/:id", put(long_term_update))
        .route("/api/memory/long_term/:id", delete(long_term_delete))
        .route("/api/memory/long_term/:id/history", get(long_term_history))
        // workspace
        .route("/api/memory/workspace/:workspace_id", get(workspace_list))
        .route("/api/memory/workspace/:workspace_id", post(workspace_save))
        .route(
            "/api/memory/workspace/:workspace_id/search",
            post(workspace_search),
        )
        .route(
            "/api/memory/workspace/:workspace_id/curate",
            post(workspace_curate),
        )
        .route(
            "/api/memory/workspace/:workspace_id/:id",
            put(workspace_update),
        )
        .route(
            "/api/memory/workspace/:workspace_id/:id",
            delete(workspace_delete),
        )
        // short-term
        .route(
            "/api/memory/short_term/:conversation_id",
            get(short_term_get),
        )
        .route(
            "/api/memory/short_term/:conversation_id",
            post(short_term_append),
        )
        .route(
            "/api/memory/short_term/:conversation_id",
            delete(short_term_clear),
        )
        // reflection
        .route("/api/memory/reflect", post(reflect))
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
    async fn long_term_list_without_token_returns_401() {
        assert_401("GET", "/api/memory/long_term", None).await;
    }

    #[tokio::test]
    async fn long_term_save_without_token_returns_401() {
        assert_401("POST", "/api/memory/long_term", Some(r#"{"body":"hello"}"#)).await;
    }

    #[tokio::test]
    async fn long_term_search_without_token_returns_401() {
        assert_401(
            "POST",
            "/api/memory/long_term/search",
            Some(r#"{"query":"x"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn long_term_update_without_token_returns_401() {
        assert_401("PUT", "/api/memory/long_term/abc", Some(r#"{"body":"y"}"#)).await;
    }

    #[tokio::test]
    async fn long_term_delete_without_token_returns_401() {
        assert_401("DELETE", "/api/memory/long_term/abc", None).await;
    }

    #[tokio::test]
    async fn long_term_history_without_token_returns_401() {
        assert_401("GET", "/api/memory/long_term/abc/history", None).await;
    }

    #[tokio::test]
    async fn workspace_list_without_token_returns_401() {
        assert_401("GET", "/api/memory/workspace/ws1", None).await;
    }

    #[tokio::test]
    async fn workspace_save_without_token_returns_401() {
        assert_401(
            "POST",
            "/api/memory/workspace/ws1",
            Some(r#"{"body":"hi"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn workspace_search_without_token_returns_401() {
        assert_401(
            "POST",
            "/api/memory/workspace/ws1/search",
            Some(r#"{"query":"x"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn workspace_curate_without_token_returns_401() {
        assert_401("POST", "/api/memory/workspace/ws1/curate", None).await;
    }

    #[tokio::test]
    async fn workspace_update_without_token_returns_401() {
        assert_401(
            "PUT",
            "/api/memory/workspace/ws1/m1",
            Some(r#"{"body":"x"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn workspace_delete_without_token_returns_401() {
        assert_401("DELETE", "/api/memory/workspace/ws1/m1", None).await;
    }

    #[tokio::test]
    async fn short_term_get_without_token_returns_401() {
        assert_401("GET", "/api/memory/short_term/c1", None).await;
    }

    #[tokio::test]
    async fn short_term_append_without_token_returns_401() {
        assert_401(
            "POST",
            "/api/memory/short_term/c1",
            Some(r#"{"entry":{"role":"user","text":"hi"}}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn short_term_clear_without_token_returns_401() {
        assert_401("DELETE", "/api/memory/short_term/c1", None).await;
    }

    #[tokio::test]
    async fn reflect_without_token_returns_401() {
        assert_401("POST", "/api/memory/reflect", Some(r#"{"scope":"daily"}"#)).await;
    }

    #[test]
    fn merged_payload_injects_path_keys_over_body() {
        let body = Some(Json(json!({"id": "from-body", "extra": 1})));
        let merged = merged_payload(body, vec![("id", Value::String("from-path".into()))]);
        assert_eq!(merged["id"], "from-path");
        assert_eq!(merged["extra"], 1);
    }

    #[test]
    fn merged_payload_handles_missing_body() {
        let merged = merged_payload(None, vec![("id", Value::String("x".into()))]);
        assert_eq!(merged["id"], "x");
    }

    #[test]
    fn query_payload_parses_booleans() {
        let mut q = HashMap::new();
        q.insert("include_superseded".into(), "true".into());
        q.insert("name".into(), "foo".into());
        let p = query_payload(q);
        assert_eq!(p["include_superseded"], true);
        assert_eq!(p["name"], "foo");
    }
}
