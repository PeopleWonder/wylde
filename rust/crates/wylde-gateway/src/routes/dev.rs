//! `/api/dev/*` — development-diagnostics surface.
//!
//! Rust port of `Gateway/routes/dev.py`. One route today:
//!
//! * `POST /api/dev/gui_error` — GUI error auto-capture sink.
//!
//! The Wylde Tauri GUI hooks `window.onerror` / `unhandledrejection` /
//! the Tauri error event channel / the error-toast path through
//! `Core/GUI/src/lib/error_sink.ts` and fire-and-forget POSTs every
//! normalized error event here. This route appends each event as one
//! JSON line to `<repo_root>/logs/gui_errors.jsonl` so the LLM agent
//! (and MCP clients) can read recent GUI errors back via the
//! `gui_errors_recent` harness tool.
//!
//! ## Auth
//!
//! Gated on `require_local` — the JSONL sink must never be reachable
//! from a mobile / VPN peer. Matches the Python route's tier.
//!
//! ## Wire format
//!
//! Success is the canonical `{ok: true, data: {recorded: true}}`
//! envelope via [`success`] — the Bucket-A IPC cleanup wrapped this
//! route so it matches the rest of the JSON surface (the original Python
//! route returned a flat `{ok: true, recorded: true}`). Error paths use
//! the canonical nested `{ok: false, error: {code, message}}` envelope
//! via [`failure`].

use std::path::PathBuf;

use axum::extract::Json;
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use wylde_shared::logging::rotating_sink;

use crate::auth::require_local;
use crate::envelopes::{failure, success};

/// Required fields on the normalized GUI error event.
const REQUIRED_FIELDS: [&str; 3] = ["timestamp_iso", "source", "message"];

/// Recognized `source` values — mirrors the enum in `error_sink.ts` and
/// `Gateway/routes/dev.py`.
const VALID_SOURCES: [&str; 5] = [
    "window_error",
    "unhandled_rejection",
    "toast_error",
    "tauri_event",
    "manual",
];

/// Wylde repo root. Honours the `WYLDE_ROOT` env var the launcher sets,
/// falling back to the process working directory.
fn repo_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Repo-root `logs/gui_errors.jsonl` — the single shared sink.
fn log_path() -> PathBuf {
    repo_root().join("logs").join("gui_errors.jsonl")
}

/// `POST /api/dev/gui_error` — record one normalized GUI error event.
///
/// Validates the normalized shape `error_sink.ts` produces
/// (`timestamp_iso` / `source` / `message` required; `stack` / `route`
/// / `severity` / `context` optional), appends it as one line to
/// `logs/gui_errors.jsonl`, and returns `{ok, recorded}`.
pub async fn gui_error(body: Option<Json<Value>>) -> Response {
    let payload = match body {
        Some(Json(Value::Object(m))) => Value::Object(m),
        _ => {
            return failure(
                "bad_request",
                "body must be a JSON object",
                StatusCode::BAD_REQUEST,
            );
        }
    };

    for field in REQUIRED_FIELDS {
        let ok = payload
            .get(field)
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !ok {
            return failure(
                "bad_request",
                &format!("'{field}' is required and must be a non-empty string"),
                StatusCode::BAD_REQUEST,
            );
        }
    }

    let source = payload.get("source").and_then(Value::as_str).unwrap_or("");
    if !VALID_SOURCES.contains(&source) {
        return failure(
            "bad_request",
            &format!("source '{source}' is not a recognized GUI error source"),
            StatusCode::BAD_REQUEST,
        );
    }

    let record = normalize(&payload);
    match append_line(&record).await {
        Ok(()) => success(json!({"recorded": true})),
        Err(err) => failure(
            "io_error",
            &format!("could not append to gui_errors.jsonl: {err}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// Project the request onto the canonical 7-key record so the JSONL
/// stays uniform for `gui_errors_recent` to parse. Required fields are
/// validated by the caller; optional ones default to `null` (`context`
/// to `{}`).
fn normalize(payload: &Value) -> Value {
    let pick = |k: &str| payload.get(k).cloned().unwrap_or(Value::Null);
    let context = match payload.get("context") {
        Some(v) if v.is_object() => v.clone(),
        _ => json!({}),
    };
    json!({
        "timestamp_iso": pick("timestamp_iso"),
        "source": pick("source"),
        "message": pick("message"),
        "stack": pick("stack"),
        "route": pick("route"),
        "severity": pick("severity"),
        "context": context,
    })
}

/// Append one JSON line to `logs/gui_errors.jsonl` via the shared
/// rotating sink, so this error log inherits the size + retention policy
/// like every other Wylde-owned sink (no per-file cap here).
///
/// **The write still flushes before returning — do not regress that.**
/// [`RotatingLog::write_line`](wylde_shared::logging::RotatingLog::write_line)
/// flushes synchronously, so a returned `Ok(())` means the record reached
/// disk. The earlier `tokio::fs` version could report success while the
/// buffered write was silently dropped at handle-drop — a ~3% flake in
/// `records_a_well_formed_event` (file created but empty). A
/// silently-dropped error report is the worst failure mode for an error
/// sink: the one thing it exists to do, failing in the one way nobody
/// would notice. The sink is synchronous, so run it on a blocking task to
/// keep it off the async runtime.
async fn append_line(record: &Value) -> std::io::Result<()> {
    let path = log_path();
    let line = serde_json::to_string(record).unwrap_or_default();
    tokio::task::spawn_blocking(move || rotating_sink(&path).write_line(&line))
        .await
        .map_err(std::io::Error::other)?
}

/// Build the `/api/dev` sub-router.
pub fn router() -> Router {
    Router::new().route(
        "/api/dev/gui_error",
        post(gui_error).route_layer(from_fn(require_local)),
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

    fn valid_event() -> Value {
        json!({
            "timestamp_iso": "2026-05-22T10:00:00Z",
            "source": "window_error",
            "message": "TypeError: cannot read properties of undefined",
            "stack": "at handleClick (Dashboard.svelte:87)",
            "route": "dashboard",
            "severity": "error",
            "context": {"handler": "startService"},
        })
    }

    async fn post_json(uri: &str, body: &Value) -> Response {
        let app = router();
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        // No ConnectInfo => client_ip defaults to 127.0.0.1 => local.
        app.oneshot(req).await.unwrap()
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn records_a_well_formed_event() {
        let tmp = tempfile::tempdir().unwrap();
        // No other test in this crate reads WYLDE_ROOT, so setting the
        // process env here is safe — the happy path is the only case
        // that reaches the filesystem write.
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let resp = post_json("/api/dev/gui_error", &valid_event()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["recorded"], true);

        let logged =
            std::fs::read_to_string(tmp.path().join("logs").join("gui_errors.jsonl")).unwrap();
        let lines: Vec<&str> = logged.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1);
        let rec: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(rec["source"], "window_error");
        assert_eq!(rec["route"], "dashboard");
        assert_eq!(rec["context"]["handler"], "startService");
    }

    #[tokio::test]
    async fn rejects_a_missing_required_field() {
        let mut event = valid_event();
        event.as_object_mut().unwrap().remove("message");
        let resp = post_json("/api/dev/gui_error", &event).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn rejects_an_unknown_source() {
        let mut event = valid_event();
        event["source"] = json!("totally_made_up");
        let resp = post_json("/api/dev/gui_error", &event).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn rejects_a_non_object_body() {
        let resp = post_json("/api/dev/gui_error", &json!(["not", "an", "object"])).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn rejects_a_non_local_caller() {
        let app = router();
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/dev/gui_error")
            .header("content-type", "application/json")
            .body(Body::from(valid_event().to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[test]
    fn normalize_defaults_optional_fields() {
        let rec = normalize(&json!({
            "timestamp_iso": "2026-05-22T10:00:00Z",
            "source": "manual",
            "message": "probe",
        }));
        assert_eq!(rec["stack"], Value::Null);
        assert_eq!(rec["route"], Value::Null);
        assert_eq!(rec["severity"], Value::Null);
        assert_eq!(rec["context"], json!({}));
    }
}
