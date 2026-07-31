//! `/api/tools` — read-only tool-registry inspection surface.
//!
//! Rust port of `Gateway/routes/tool_registry.py`. Python imports the
//! tool catalog in-process from `Core.harness.tooling.tool_registry`;
//! Rust has no in-process Python access, so this port dispatches the
//! harness `tools.list` pipe action and **reshapes** the canonical list
//! reply into the alias-keyed dict shape Python returns.
//!
//! Alias keys (matches Python's `_alias_keys_for(entry)`):
//!
//! * `id`                      — e.g. `memory_long_term_save`
//! * `id.replace("_", ".")`    — e.g. `memory.long_term.save`
//! * `name`                    — the dotted action name
//! * `name.replace(".", "_")`  — the snake-case mirror
//!
//! Same entry is stored under every alias so callers can look up tools
//! by either spelling. The `count` field equals the dict's len.

use std::collections::BTreeMap;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use super::common::{envelope_to_response, harness_dispatch};
use crate::auth::require_local;
use crate::envelopes::{failure, success};
use crate::proxy_core::pipe_action;

/// `GET /api/tools` — return every registered tool, alias-keyed.
pub async fn list_all() -> Response {
    let resp = harness_dispatch("tools.list", json!({})).await;
    let (parts, body_bytes) = match response_into_parts(resp).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !parts.status.is_success() {
        return rebuild_failure(parts, body_bytes);
    }
    let parsed: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return failure(
                "bad_gateway",
                &format!("harness reply was not JSON: {e}"),
                StatusCode::BAD_GATEWAY,
            );
        }
    };
    // `harness_dispatch` already wrapped the success path in {ok, data}.
    // The shape we need is `{count, tools: <alias-keyed dict>}`, so dig
    // into `data` to pull out the canonical list, then reshape.
    let canonical = extract_canonical_list(&parsed);
    let alias_keyed = reshape_to_alias_keyed(&canonical);
    success(json!({
        "count": alias_keyed.len(),
        "tools": alias_keyed,
    }))
}

/// `GET /api/tools/{tool_id}` — return a single tool by id or alias.
pub async fn get_one(Path(tool_id): Path<String>) -> Response {
    // Defer to the harness if it has a dedicated `tools.get` action;
    // otherwise fall back to filtering `tools.list`. The probe is
    // intentional: the `not_implemented`/`unknown_action` arm below
    // falls back to `tools.list`, so an unregistered verb here is fine.
    // wylde-check: optional-verb
    match pipe_action("wylde-harness", "tools.get", json!({ "tool_id": tool_id })).await {
        Ok(data) => {
            // Successful lookup — wrap and return.
            success(data)
        }
        Err((status, body)) => {
            // Some harness versions don't implement `tools.get`; on a
            // `not_implemented` reply fall back to filtering `tools.list`.
            let code = body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if status == StatusCode::NOT_FOUND || code == "tool_not_found" {
                return failure("tool_not_found", &tool_id, StatusCode::NOT_FOUND);
            }
            if code == "not_implemented" || code == "unknown_action" {
                return fallback_get_via_list(&tool_id).await;
            }
            envelope_to_response((status, body))
        }
    }
}

async fn fallback_get_via_list(tool_id: &str) -> Response {
    let list = harness_dispatch("tools.list", json!({})).await;
    let (parts, body_bytes) = match response_into_parts(list).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !parts.status.is_success() {
        return rebuild_failure(parts, body_bytes);
    }
    let parsed: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return failure(
                "bad_gateway",
                &format!("harness reply was not JSON: {e}"),
                StatusCode::BAD_GATEWAY,
            );
        }
    };
    let canonical = extract_canonical_list(&parsed);
    let alias_keyed = reshape_to_alias_keyed(&canonical);
    match alias_keyed.get(tool_id) {
        Some(entry) => success(entry.clone()),
        None => failure("tool_not_found", tool_id, StatusCode::NOT_FOUND),
    }
}

/// Pull `data.tools` out of a `harness_dispatch` success envelope. Python's
/// `tools.list` returns either a list or a dict (different harness
/// versions); we handle both.
fn extract_canonical_list(reply: &Value) -> Vec<Value> {
    let data = reply.get("data").unwrap_or(reply);
    if let Some(tools) = data.get("tools") {
        return match tools {
            Value::Array(a) => a.clone(),
            Value::Object(m) => m.values().cloned().collect(),
            _ => Vec::new(),
        };
    }
    match data {
        Value::Array(a) => a.clone(),
        Value::Object(m) => {
            if m.values().all(|v| v.is_object()) {
                m.values().cloned().collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Derive every alias key for a canonical entry and emit the dict shape.
///
/// Uses a `BTreeMap` so the on-wire order is deterministic — clients
/// that diff the JSON across builds don't see spurious reordering.
fn reshape_to_alias_keyed(entries: &[Value]) -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for entry in entries {
        for key in alias_keys_for(entry) {
            out.insert(key, entry.clone());
        }
    }
    out
}

/// Match Python's `_alias_keys_for(entry)`: collect every spelling the
/// catalog accepts as a lookup key for one tool.
fn alias_keys_for(entry: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(4);
    if let Some(id) = entry.get("id").and_then(Value::as_str) {
        push_unique(&mut out, id.to_owned());
        push_unique(&mut out, id.replace('_', "."));
    }
    if let Some(name) = entry.get("name").and_then(Value::as_str) {
        push_unique(&mut out, name.to_owned());
        push_unique(&mut out, name.replace('.', "_"));
    }
    out
}

fn push_unique(out: &mut Vec<String>, key: String) {
    if !key.is_empty() && !out.contains(&key) {
        out.push(key);
    }
}

// ── Response surgery helpers ──────────────────────────────────────────
//
// `harness_dispatch` returns an `axum::Response` because most routes
// just pass it through. Here we need to inspect / reshape the body, so
// dismantle it and rebuild.

async fn response_into_parts(
    resp: Response,
) -> Result<(axum::http::response::Parts, axum::body::Bytes), Response> {
    let (parts, body) = resp.into_parts();
    match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => Ok((parts, b)),
        Err(e) => Err(failure(
            "bad_gateway",
            &format!("could not read harness reply: {e}"),
            StatusCode::BAD_GATEWAY,
        )),
    }
}

fn rebuild_failure(parts: axum::http::response::Parts, body: axum::body::Bytes) -> Response {
    let mut builder = Response::builder().status(parts.status);
    for (k, v) in parts.headers.iter() {
        builder = builder.header(k, v);
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| failure("bad_gateway", "rebuild failed", StatusCode::BAD_GATEWAY))
}

pub fn router() -> Router {
    Router::new()
        .route(
            "/api/tools",
            get(list_all).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/tools/{tool_id}",
            get(get_one).route_layer(from_fn(require_local)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    #[test]
    fn alias_keys_cover_both_spellings() {
        let entry = json!({
            "id": "memory_long_term_save",
            "name": "memory.long_term.save",
        });
        let keys = alias_keys_for(&entry);
        assert!(keys.contains(&"memory_long_term_save".to_owned()));
        assert!(keys.contains(&"memory.long_term.save".to_owned()));
    }

    #[test]
    fn alias_keys_dedupes_when_id_and_name_already_overlap() {
        // The `id_swap_to_dotted` and `name` derivations both produce
        // the dotted spelling — the helper must keep one copy.
        let entry = json!({
            "id": "rag_ask",
            "name": "rag.ask",
        });
        let keys = alias_keys_for(&entry);
        assert!(keys.contains(&"rag.ask".to_owned()));
        let dotted_count = keys.iter().filter(|k| *k == "rag.ask").count();
        assert_eq!(dotted_count, 1, "dedup failed: {keys:?}");
    }

    #[test]
    fn reshape_stores_same_entry_under_every_alias() {
        let entries = vec![json!({"id": "x_y", "name": "x.y", "kind": "test"})];
        let dict = reshape_to_alias_keyed(&entries);
        assert_eq!(dict.get("x_y").and_then(|v| v.get("kind")).unwrap(), "test");
        assert_eq!(dict.get("x.y").and_then(|v| v.get("kind")).unwrap(), "test");
    }

    #[test]
    fn extract_canonical_list_handles_array_under_tools_field() {
        let reply = json!({"ok": true, "data": {"tools": [{"id": "a"}, {"id": "b"}]}});
        let v = extract_canonical_list(&reply);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn extract_canonical_list_handles_bare_array() {
        let reply = json!({"data": [{"id": "a"}]});
        let v = extract_canonical_list(&reply);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn extract_canonical_list_handles_dict_keyed_by_id() {
        let reply = json!({"data": {"a": {"id": "a"}, "b": {"id": "b"}}});
        let v = extract_canonical_list(&reply);
        assert_eq!(v.len(), 2);
    }

    #[tokio::test]
    async fn list_rejects_non_local_caller() {
        let app = router();
        let mut req = Request::builder()
            .uri("/api/tools")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn get_rejects_non_local_caller() {
        let app = router();
        let mut req = Request::builder()
            .uri("/api/tools/foo")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
