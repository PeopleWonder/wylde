//! HTTP front door — the N8N-facing transport.
//!
//! N8N nodes speak HTTP/JS and cannot open a Windows named pipe, so the
//! sidecar binds a **loopback** TCP listener alongside the pipe (the same
//! belt-and-suspenders shape `memgraph.py` uses: pipe canonical, HTTP for
//! N8N). The ingest workflow's chunking node is an N8N **HTTP Request** node
//! pointed at `POST http://127.0.0.1:<port>/chunk` — no Python adapter, no
//! Execute-Command CLI shim (see `docs/plans/treesitter-sidecar.md`
//! §Integration).
//!
//! Every route is a thin envelope over the SAME handler fn the pipe action
//! surface uses (`service::handle_chunk`, `parser::*`), so the HTTP route and
//! the action verb can never drift. Mirrors `wylde-vpn/src/http.rs`.
//!
//! Routes:
//!   * `GET  /health`            — liveness + linked-grammar count.
//!   * `GET  /languages`         — `{languages:[{name, grammar_sha, abi}]}`.
//!   * `POST /chunk`             — `{path, language?, max_chunk_bytes?}` → chunk list.
//!   * `POST /extract_entities`  — `{path, language?}` → functions/classes/imports/calls (the Memgraph graph feed).
//!   * `POST /outline`           — `{path, language?}` → nested symbol tree (TBS Slice H).
//!   * `POST /highlight`         — `{path, language?}` → syntax-highlight spans (TBS Slice H).

use std::net::SocketAddr;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::{parser, service};

/// Build the axum router. Pulled out from [`serve`] so unit tests can hit the
/// routes via `tower::ServiceExt` without binding a port.
pub fn router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/languages", get(languages_route))
        .route("/chunk", post(chunk_route))
        .route("/extract_entities", post(extract_entities_route))
        .route("/outline", post(outline_route))
        .route("/highlight", post(highlight_route))
}

/// Bind `127.0.0.1:<port>` (loopback only — the chunk surface is never exposed
/// beyond the host, per plan risk #1) and serve until cancelled.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("wylde-treesitter: HTTP front door listening on {addr}");
    axum::serve(listener, router()).await?;
    Ok(())
}

// ── Route handlers ─────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(json!({
        "status": "healthy",
        "service": "wylde-treesitter",
        "grammars": parser::REGISTRY.iter().map(|g| g.name).collect::<Vec<_>>(),
        "impl": "rust",
    }))
}

async fn languages_route() -> Response {
    // `parser::languages()` already returns the `{languages:[…]}` payload.
    (StatusCode::OK, Json(parser::languages())).into_response()
}

async fn chunk_route(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    reply_to_response(service::handle_chunk(payload))
}

async fn extract_entities_route(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    reply_to_response(service::handle_extract_entities(payload))
}

async fn outline_route(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    reply_to_response(service::handle_outline(payload))
}

async fn highlight_route(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(Value::Null);
    reply_to_response(service::handle_highlight(payload))
}

// ── helpers ──────────────────────────────────────────────────────────────

/// Map an action [`Reply`] onto an axum response — the same envelope shape
/// `wylde-vpn` uses: `ok=true` → 200 with `data` as the body; `ok=false` →
/// an `{error, code}` body with a status derived from the error code.
fn reply_to_response(reply: Reply) -> Response {
    if reply.ok {
        return (StatusCode::OK, Json(reply.data)).into_response();
    }
    let err = reply
        .error
        .unwrap_or_else(|| wylde_shared::ipc::IpcError::new("unknown", "unknown error"));
    let status = match err.code.as_str() {
        "invalid_request" | "bad_request" => StatusCode::BAD_REQUEST,
        "unknown_language" | "unsupported_language" => StatusCode::UNPROCESSABLE_ENTITY,
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
    use axum::body::Body;
    use axum::http::Request;
    use std::io::Write;
    use tower::ServiceExt; // for `oneshot`

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_route_reports_healthy() {
        let resp = router()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["service"], "wylde-treesitter");
        assert!(v["grammars"].as_array().unwrap().contains(&json!("python")));
    }

    #[tokio::test]
    async fn languages_route_lists_every_grammar() {
        let resp = router()
            .oneshot(Request::get("/languages").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let langs = v["languages"].as_array().unwrap();
        assert_eq!(langs.len(), 10);
        assert_eq!(langs[0]["name"], "python");
        let names: Vec<&str> = langs.iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"tsx"), "tsx grammar missing: {names:?}");
    }

    #[tokio::test]
    async fn chunk_route_chunks_a_python_file() {
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"def a():\n    return 1\n").unwrap();
        f.flush().unwrap();
        let payload = json!({ "path": f.path().to_str().unwrap() });
        let resp = router()
            .oneshot(
                Request::post("/chunk")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ast_aware"], true);
        assert_eq!(v["chunks"][0]["symbol_name"], "a");
    }

    #[tokio::test]
    async fn extract_entities_route_returns_structure() {
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"import os\n\nclass C(Base):\n    def m(self):\n        helper()\n")
            .unwrap();
        f.flush().unwrap();
        let payload = json!({ "path": f.path().to_str().unwrap() });
        let resp = router()
            .oneshot(
                Request::post("/extract_entities")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["classes"][0]["name"], "C");
        assert_eq!(v["classes"][0]["bases"][0], "Base");
        assert_eq!(v["classes"][0]["methods"][0], "m");
        assert_eq!(v["imports"][0]["module"], "os");
        // The call inside the method is attributed to the method.
        let calls = v["calls"].as_array().unwrap();
        assert!(calls
            .iter()
            .any(|c| c["callee"] == "helper" && c["caller"] == "m"));
    }

    #[tokio::test]
    async fn outline_route_returns_the_tree() {
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"class C:\n    def m(self):\n        pass\n")
            .unwrap();
        f.flush().unwrap();
        let payload = json!({ "path": f.path().to_str().unwrap() });
        let resp = router()
            .oneshot(
                Request::post("/outline")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["tree"][0]["name"], "C");
        assert_eq!(v["tree"][0]["children"][0]["name"], "m");
    }

    #[tokio::test]
    async fn highlight_route_returns_spans() {
        let mut f = tempfile::Builder::new().suffix(".rs").tempfile().unwrap();
        f.write_all(b"fn main() { let s = \"x\"; }\n").unwrap();
        f.flush().unwrap();
        let payload = json!({ "path": f.path().to_str().unwrap() });
        let resp = router()
            .oneshot(
                Request::post("/highlight")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v["span_count"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn chunk_route_missing_path_is_400() {
        let resp = router()
            .oneshot(
                Request::post("/chunk")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
