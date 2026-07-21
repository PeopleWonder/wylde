//! Shared helpers for the wave-2a route family.
//!
//! Bearer-token extraction and the `(StatusCode, Value)` → [`Response`]
//! conversion that `proxy_core::pipe_action` and `proxy_core::validate_token`
//! both return. Lives here so the chat / conversations / prompts routes
//! share one implementation instead of growing slightly-different copies.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::auth::extract_bearer;
use crate::envelopes::failure;
use crate::proxy_core::{pipe_action, validate_token, ProxyResult};

/// Harness pipe service name. The chat / conversations / prompts routes
/// all dispatch into `\\.\pipe\wylde-harness`.
pub(super) const HARNESS_SERVICE: &str = "wylde-harness";

/// Workspaces pipe service name. The `workspaces.*` verbs live on their
/// own service pipe (`\\.\pipe\wylde-workspaces`) — the harness RETIRED
/// them (Thought Bubble System Slice 0d), so dispatching them to
/// `wylde-harness` returns `no_action`.
pub(super) const WORKSPACES_SERVICE: &str = "wylde-workspaces";

/// Translate the `(status, envelope)` tuple `proxy_core` returns on its
/// error path into an axum [`Response`] carrying the canonical
/// `{ok: false, error: {code, message}}` body.
pub(super) fn envelope_to_response(env: (StatusCode, Value)) -> Response {
    let (status, body) = env;
    let code = body
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("error")
        .to_owned();
    let message = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    failure(&code, &message, status)
}

/// Verify the request carries a Bearer token that device-gate accepts.
/// On the error path returns the [`Response`] the handler should return
/// verbatim; on success returns the verified-device payload from
/// device-gate (currently unused, but kept on the return type so future
/// routes can read `device_id` / `tier` without revalidating).
pub(super) async fn authorize(headers: &HeaderMap) -> Result<Value, Response> {
    let token = match extract_bearer(headers) {
        Some(t) => t,
        None => {
            return Err(failure(
                "missing_token",
                "Bearer token required (Authorization: Bearer <token>)",
                StatusCode::UNAUTHORIZED,
            ));
        }
    };
    let result: ProxyResult = validate_token(&token).await;
    result.map_err(envelope_to_response)
}

/// Fire an action on `wylde-harness` and shape the [`Response`].
///
/// On success, the action's reply data is wrapped in the standard
/// `{ok: true, data: …}` envelope. On failure, the `(status, body)`
/// tuple from [`pipe_action`] is converted via [`envelope_to_response`].
pub(super) async fn harness_dispatch(action: &str, payload: Value) -> Response {
    match pipe_action(HARNESS_SERVICE, action, payload).await {
        Ok(data) => crate::envelopes::success(data),
        Err(env) => envelope_to_response(env),
    }
}

/// Fire an action on `wylde-workspaces` and shape the [`Response`].
///
/// The workspace CRUD/MRU/persona verbs (`workspaces.*`) are served by the
/// wylde-workspaces service pipe, not the harness — see
/// [`WORKSPACES_SERVICE`]. Otherwise identical to [`harness_dispatch`].
pub(super) async fn workspaces_dispatch(action: &str, payload: Value) -> Response {
    match pipe_action(WORKSPACES_SERVICE, action, payload).await {
        Ok(data) => crate::envelopes::success(data),
        Err(env) => envelope_to_response(env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn bearer_happy_path() {
        let h = headers_with("Bearer abc.def");
        assert_eq!(extract_bearer(&h), Some("abc.def".to_owned()));
    }

    #[test]
    fn bearer_case_insensitive_scheme() {
        let h = headers_with("bearer xyz");
        assert_eq!(extract_bearer(&h), Some("xyz".to_owned()));
    }

    #[test]
    fn bearer_rejects_other_scheme() {
        let h = headers_with("Basic xyz");
        assert_eq!(extract_bearer(&h), None);
    }

    #[test]
    fn bearer_rejects_empty_token() {
        let h = headers_with("Bearer ");
        assert_eq!(extract_bearer(&h), None);
    }

    #[test]
    fn bearer_missing_header() {
        let h = HeaderMap::new();
        assert_eq!(extract_bearer(&h), None);
    }

    #[test]
    fn envelope_carries_code_and_message() {
        let v = serde_json::json!({
            "ok": false,
            "error": {"code": "bad_request", "message": "id missing"}
        });
        let resp = envelope_to_response((StatusCode::BAD_REQUEST, v));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn envelope_handles_missing_error_block() {
        let resp = envelope_to_response((StatusCode::BAD_GATEWAY, serde_json::json!({})));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
