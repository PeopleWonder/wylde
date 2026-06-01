//! Streamable HTTP transport for the MCP surface.
//!
//! Rust port of `Gateway/routes/mcp/transport.py`. One endpoint
//! (`/mcp`) per the MCP Streamable HTTP transport. This module owns
//! everything between the raw HTTP body and [`super::handlers::dispatch`]:
//! JSON-RPC framing, request validation, the notification/request
//! split, and session tracking.
//!
//! ## Sessions
//!
//! A session id is minted on `initialize` and returned in the
//! `Mcp-Session-Id` response header. Subsequent requests MAY echo it;
//! the store records `last_seen` so a future server-initiated stream
//! has per-session context to hang off. The transport is deliberately
//! lenient — a request without a session id is still served (stateless
//! fallback), so a minimal client that ignores the header keeps working.
//!
//! `GET /mcp` would open the server-to-client SSE stream; v1 emits no
//! server-initiated messages, so `GET` returns `405` per the spec.
//!
//! MCP spec: <https://spec.modelcontextprotocol.io/> (revision 2025-06-18).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use axum::http::StatusCode;
use serde_json::{json, Value};

use super::handlers;

/// Request header carrying the session id (lookup is case-insensitive).
pub const SESSION_HEADER: &str = "mcp-session-id";
/// JSON-RPC protocol version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Idle sessions older than this are pruned on the next mint.
const SESSION_TTL_SECONDS: u64 = 3600;

/// Per-session bookkeeping. The store only needs `last_seen` for the
/// idle-TTL sweep; the Python port additionally keeps `created_at` for
/// the session-context dict it hands to handlers.
struct SessionMeta {
    last_seen: Instant,
}

fn sessions() -> &'static Mutex<HashMap<String, SessionMeta>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, SessionMeta>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Mint a new session id, prune stale entries, and record it.
pub fn create_session() -> String {
    let sid = uuid::Uuid::new_v4().as_simple().to_string();
    let now = Instant::now();
    let mut store = sessions().lock().expect("mcp session store poisoned");
    store.retain(|_, meta| now.duration_since(meta.last_seen).as_secs() <= SESSION_TTL_SECONDS);
    store.insert(sid.clone(), SessionMeta { last_seen: now });
    sid
}

/// Touch a session's `last_seen` if it exists. Returns whether it was
/// known — `process_post` ignores the result (stateless fallback), but
/// tests assert on it.
pub fn touch_session(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let mut store = sessions().lock().expect("mcp session store poisoned");
    match store.get_mut(session_id) {
        Some(meta) => {
            meta.last_seen = Instant::now();
            true
        }
        None => false,
    }
}

// ── JSON-RPC framing ───────────────────────────────────────────────────

fn success_frame(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result })
}

fn error_frame(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut err = json!({ "code": code, "message": message });
    if let Some(extra) = data {
        if let Some(map) = err.as_object_mut() {
            map.insert("data".to_owned(), extra);
        }
    }
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "error": err })
}

/// JSON-RPC error body for `GET /mcp` — v1 has no server-initiated stream.
pub fn unsupported_get() -> Value {
    error_frame(
        &Value::Null,
        handlers::INVALID_REQUEST,
        "GET is not supported; the v1 MCP surface has no server-initiated stream",
        None,
    )
}

/// Result of processing one `POST /mcp` body.
pub struct PostOutcome {
    /// JSON-RPC response body, or `None` for a notification (the caller
    /// answers `202` with an empty body).
    pub body: Option<Value>,
    pub status: StatusCode,
    /// Set only for `initialize` — surfaced as `Mcp-Session-Id`.
    pub new_session: Option<String>,
}

impl PostOutcome {
    fn err(id: &Value, code: i64, message: &str, status: StatusCode) -> Self {
        Self {
            body: Some(error_frame(id, code, message, None)),
            status,
            new_session: None,
        }
    }
}

/// Process one `POST /mcp` body.
pub async fn process_post(raw_body: &[u8], session_id: Option<&str>) -> PostOutcome {
    let payload: Value = match serde_json::from_slice(raw_body) {
        Ok(v) => v,
        Err(_) => {
            return PostOutcome::err(
                &Value::Null,
                handlers::PARSE_ERROR,
                "Parse error: request body is not valid JSON",
                StatusCode::BAD_REQUEST,
            );
        }
    };

    if payload.is_array() {
        return PostOutcome::err(
            &Value::Null,
            handlers::INVALID_REQUEST,
            "JSON-RPC batching is not supported (removed in MCP 2025-06-18)",
            StatusCode::BAD_REQUEST,
        );
    }
    let obj = match payload.as_object() {
        Some(o) => o,
        None => {
            return PostOutcome::err(
                &Value::Null,
                handlers::INVALID_REQUEST,
                "Invalid Request: expected a JSON-RPC object",
                StatusCode::BAD_REQUEST,
            );
        }
    };

    let is_notification = !obj.contains_key("id");
    let req_id = obj.get("id").cloned().unwrap_or(Value::Null);

    let method = match obj.get("method").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m.to_owned(),
        _ => {
            if is_notification {
                return PostOutcome {
                    body: None,
                    status: StatusCode::ACCEPTED,
                    new_session: None,
                };
            }
            return PostOutcome::err(
                &req_id,
                handlers::INVALID_REQUEST,
                "Invalid Request: 'method' is required",
                StatusCode::BAD_REQUEST,
            );
        }
    };

    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));
    let params = if params.is_object() { params } else { json!({}) };

    // A session is minted on initialize; other methods are served with
    // whatever session the client echoed (or none — stateless fallback).
    let mut new_session: Option<String> = None;
    if method == "initialize" {
        new_session = Some(create_session());
    } else if let Some(sid) = session_id {
        touch_session(sid);
    }

    let dispatched = handlers::dispatch(&method, &params).await;
    if is_notification {
        return PostOutcome {
            body: None,
            status: StatusCode::ACCEPTED,
            new_session,
        };
    }
    match dispatched {
        Ok(result) => PostOutcome {
            body: Some(success_frame(&req_id, result)),
            status: StatusCode::OK,
            new_session,
        },
        Err(err) => PostOutcome {
            body: Some(error_frame(&req_id, err.code, &err.message, err.data)),
            status: StatusCode::OK,
            new_session,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_mints_unique_ids() {
        let a = create_session();
        let b = create_session();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32, "uuid simple form is 32 hex chars");
    }

    #[test]
    fn touch_session_tracks_known_ids_only() {
        let sid = create_session();
        assert!(touch_session(&sid));
        assert!(!touch_session("never-issued"));
        assert!(!touch_session(""));
    }

    #[tokio::test]
    async fn process_post_rejects_non_json_body() {
        let outcome = process_post(b"not json at all", None).await;
        assert_eq!(outcome.status, StatusCode::BAD_REQUEST);
        let body = outcome.body.unwrap();
        assert_eq!(body["error"]["code"], handlers::PARSE_ERROR);
        assert_eq!(body["id"], Value::Null);
    }

    #[tokio::test]
    async fn process_post_rejects_json_rpc_batch_array() {
        let outcome = process_post(b"[{\"jsonrpc\":\"2.0\"}]", None).await;
        assert_eq!(outcome.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            outcome.body.unwrap()["error"]["code"],
            handlers::INVALID_REQUEST
        );
    }

    #[tokio::test]
    async fn process_post_rejects_request_without_method() {
        let raw = br#"{"jsonrpc":"2.0","id":7}"#;
        let outcome = process_post(raw, None).await;
        assert_eq!(outcome.status, StatusCode::BAD_REQUEST);
        let body = outcome.body.unwrap();
        assert_eq!(body["error"]["code"], handlers::INVALID_REQUEST);
        assert_eq!(body["id"], 7);
    }

    #[tokio::test]
    async fn process_post_initialize_returns_result_and_mints_session() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let outcome = process_post(raw, None).await;
        assert_eq!(outcome.status, StatusCode::OK);
        assert!(outcome.new_session.is_some());
        let body = outcome.body.unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(
            body["result"]["protocolVersion"],
            handlers::MCP_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn process_post_notification_yields_202_and_no_body() {
        let raw = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let outcome = process_post(raw, None).await;
        assert_eq!(outcome.status, StatusCode::ACCEPTED);
        assert!(outcome.body.is_none());
    }

    #[tokio::test]
    async fn process_post_unknown_method_is_a_200_json_rpc_error() {
        let raw = br#"{"jsonrpc":"2.0","id":"x","method":"bogus"}"#;
        let outcome = process_post(raw, None).await;
        // JSON-RPC application errors ride a 200 — the request was
        // well-formed, the method just isn't one we serve.
        assert_eq!(outcome.status, StatusCode::OK);
        let body = outcome.body.unwrap();
        assert_eq!(body["id"], "x");
        assert_eq!(body["error"]["code"], handlers::METHOD_NOT_FOUND);
    }

    #[test]
    fn unsupported_get_is_a_json_rpc_error() {
        let body = unsupported_get();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["error"]["code"], handlers::INVALID_REQUEST);
    }
}
