//! Standard response envelopes — Rust port of `Gateway/envelopes.py`.
//!
//! Every Gateway response follows the `{ok, data?, error?}` envelope so
//! HTTP and pipe callers see one shape. Most code inlines these via
//! `serde_json::json!` but a couple of routes prefer the helper form.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;
use serde_json::Value;

/// `ok: true` envelope. Field order matches Python's `JSONResponse(
/// {"ok": True, "data": ...})` insertion order — `ok` precedes `data`
/// on the wire. serde_json without the `preserve_order` feature sorts
/// `Map` keys alphabetically, so we use a typed struct here instead of
/// `json!` to guarantee the byte sequence.
#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    ok: bool,
    data: &'a Value,
}

#[derive(Serialize)]
struct FailureEnvelope<'a> {
    ok: bool,
    error: ErrorBlock<'a>,
}

#[derive(Serialize)]
struct ErrorBlock<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

/// Build an `ok: true` JSON response wrapping `data`.
pub fn success(data: Value) -> Response {
    success_with_status(data, StatusCode::OK)
}

pub fn success_with_status(data: Value, status: StatusCode) -> Response {
    let envelope = SuccessEnvelope {
        ok: true,
        data: &data,
    };
    // Pass the typed struct directly to `Json` so serde serializes
    // fields in declaration order. Going through `serde_json::to_value`
    // would round-trip through a `Map` whose default key ordering is
    // alphabetic, and the resulting bytes wouldn't match Python's
    // `{"ok": True, "data": ...}` insertion order.
    (status, Json(envelope)).into_response()
}

/// Build an `ok: false` JSON response with the standard error block.
pub fn failure(code: &str, message: &str, status: StatusCode) -> Response {
    failure_with_detail(code, message, status, None)
}

pub fn failure_with_detail(
    code: &str,
    message: &str,
    status: StatusCode,
    detail: Option<Value>,
) -> Response {
    let envelope = FailureEnvelope {
        ok: false,
        error: ErrorBlock {
            code,
            message,
            details: detail.as_ref(),
        },
    };
    (status, Json(envelope)).into_response()
}
