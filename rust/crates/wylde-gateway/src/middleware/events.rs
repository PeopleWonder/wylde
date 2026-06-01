//! Events middleware — forwards device-state events on response headers.
//!
//! Rust port of `Gateway/events.py`. device_gate buffers events per
//! device — `token_rotated` when the GUI rotates a device's token,
//! `revoked` when the device is revoked, `tier_changed` when the tier is
//! escalated or dropped. Without a delivery channel those events sit on
//! the queue and the mobile app finds out the hard way (next request
//! returns 401, app prompts the user to re-pair).
//!
//! The `X-Wylde-Events` response header bridges that gap: after the
//! route handler runs, [`forward_device_events`] drains the verified
//! device's pending-event queue and serialises the events as a compact
//! JSON array on that header. Mobile dispatches by the `type` field.
//!
//! ## Design vs Python
//!
//! Python split this across `stash_pending_events` (drains the queue
//! inside the `require_device` dependency) and `attach_pending_events`
//! (called by each route handler on its way out) — the explicit-request
//! API was a workaround for Starlette's middleware not exposing the
//! request to a response-side hook. axum's `from_fn` middleware *does*
//! wrap the response, so the Rust port collapses both halves into one
//! layer: drain + attach happen together after `next.run`.
//!
//! ## Wiring
//!
//! [`forward_device_events`] is an [`axum::middleware::from_fn`] layer
//! mounted **inner to `require_device`** on the device-tier routes
//! (`/api/chat/run_turn`, `/api/devices/me`) so the verified [`Device`]
//! is already in the request extensions when it runs. Being a per-route
//! layer it sits inside the global `audit_log` layer — the header is set
//! before audit observes the response. On a route with no device
//! (e.g. `/health`) the layer is a graceful no-op.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use crate::auth::Device;
use crate::services::device_gate;

/// Response header carrying the compact-JSON event array. Header names
/// are case-insensitive; insertion uses the lowercase form.
pub const EVENTS_HEADER: &str = "X-Wylde-Events";

/// `axum::middleware::from_fn` layer — drain the verified device's
/// pending-event queue and forward the events on the `X-Wylde-Events`
/// response header.
///
/// Mount inner to `require_device` so the [`Device`] extension is
/// populated. On a route with no device the layer is a no-op: the
/// response goes out untouched.
pub async fn forward_device_events(req: Request, next: Next) -> Response {
    let device_id = req
        .extensions()
        .get::<Device>()
        .map(|d| d.device_id.clone());

    let mut response = next.run(req).await;

    let Some(device_id) = device_id else {
        // No verified device (e.g. /health) — nothing to forward.
        return response;
    };

    let events = drain_pending_events(&device_id).await;
    attach_events_to_response(&mut response, &events);
    response
}

/// Drain device_gate's pending-event queue for `device_id`.
///
/// Best-effort, mirroring `events.py::stash_pending_events`: a device_gate
/// error logs and yields an empty list so the response still goes out,
/// just without the header — never a 5xx on a device_gate hiccup.
async fn drain_pending_events(device_id: &str) -> Vec<Value> {
    match device_gate::consume_pending_events(device_id).await {
        Ok(data) => extract_events(&data),
        Err((status, _)) => {
            tracing::warn!(
                "events: consume_pending_events({device_id}) failed ({}) — \
                 sending response without {EVENTS_HEADER}",
                status.as_u16()
            );
            Vec::new()
        }
    }
}

/// Pull the `events` array out of a device_gate reply, keeping only the
/// object entries — mirrors Python's `[e for e in raw if isinstance(e,
/// dict)]`. A missing/non-list `events` field, or a non-object reply,
/// yields an empty list.
fn extract_events(data: &Value) -> Vec<Value> {
    data.get("events")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter(|e| e.is_object()).cloned().collect())
        .unwrap_or_default()
}

/// Attach `events` to `response` as the `X-Wylde-Events` header. No-op
/// when `events` is empty — the header is omitted to keep responses
/// lean, matching `events.py::attach_pending_events`.
fn attach_events_to_response(response: &mut Response, events: &[Value]) {
    if events.is_empty() {
        return;
    }
    // Compact JSON array, no whitespace — Python's
    // `json.dumps(events, separators=(",", ":"))`. serde_json's default
    // formatter is already separator-compact, so this is byte-equivalent
    // for the ASCII event payloads device_gate emits.
    let encoded = match serde_json::to_string(events) {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!("events: failed to JSON-encode pending events; dropping");
            return;
        }
    };
    match HeaderValue::from_str(&encoded) {
        Ok(value) => {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-wylde-events"), value);
        }
        Err(_) => {
            tracing::warn!("events: encoded events not header-safe; dropping");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use tower::ServiceExt;

    // ── extract_events — the device_gate reply → events seam ───────────

    #[test]
    fn extract_events_pulls_objects_from_a_device_gate_reply() {
        // Shape mirrors `_consume_stub`'s `{"events": [...], "count": N}`.
        let reply = json!({
            "events": [
                {"type": "token_rotated", "new_token": "tok-A-new-9876"},
                {"type": "revoked", "device_id": "dev_A"},
            ],
            "count": 2,
        });
        let events = extract_events(&reply);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "token_rotated");
        assert_eq!(events[1]["type"], "revoked");
    }

    #[test]
    fn extract_events_filters_non_object_entries() {
        // Python keeps only dict entries; scalars / arrays are dropped.
        let reply = json!({"events": [{"type": "tier_changed"}, "junk", 42, ["x"]]});
        let events = extract_events(&reply);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "tier_changed");
    }

    #[test]
    fn extract_events_missing_or_malformed_field_is_empty() {
        assert!(extract_events(&json!({"count": 0})).is_empty());
        assert!(extract_events(&json!({"events": "not-a-list"})).is_empty());
        assert!(extract_events(&json!(null)).is_empty());
        assert!(extract_events(&json!("not-an-object")).is_empty());
    }

    // ── attach_events_to_response — header (de)serialisation ───────────

    /// Device with pending events → header present, compact JSON, and
    /// it round-trips back through a JSON decode. The `next.run` →
    /// `consume_pending_events` pipe hop can't be exercised in a unit
    /// test, so this drives the header-insertion path directly with the
    /// events `extract_events` would have produced.
    #[test]
    fn attach_events_sets_compact_json_header_that_decodes() {
        let events = vec![
            json!({"type": "token_rotated", "new_token": "tok-A-new-9876"}),
            json!({"type": "tier_changed", "tier": "tool_use"}),
        ];
        let mut response = Response::new(Body::empty());
        attach_events_to_response(&mut response, &events);

        let header = response
            .headers()
            .get("x-wylde-events")
            .expect("X-Wylde-Events must be set when events are pending")
            .to_str()
            .unwrap();
        // Compact — no whitespace after `,` or `:`.
        assert!(!header.contains(", "), "header must be compact: {header}");
        assert!(!header.contains(": "), "header must be compact: {header}");

        let decoded: Value = serde_json::from_str(header).unwrap();
        let arr = decoded.as_array().expect("header decodes to a JSON array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "token_rotated");
        assert_eq!(arr[0]["new_token"], "tok-A-new-9876");
        assert_eq!(arr[1]["type"], "tier_changed");
    }

    #[test]
    fn attach_events_with_no_pending_events_omits_header() {
        let mut response = Response::new(Body::empty());
        attach_events_to_response(&mut response, &[]);
        assert!(
            response.headers().get("x-wylde-events").is_none(),
            "empty queue must omit the header entirely"
        );
    }

    // ── forward_device_events — middleware integration ─────────────────

    async fn ok_handler() -> &'static str {
        "reached"
    }

    /// No device in the request extensions (an unauthenticated route
    /// like `/health`) → the layer is a no-op: the handler is reached,
    /// the response is 200, and no `X-Wylde-Events` header is added.
    #[tokio::test]
    async fn forward_device_events_is_a_noop_without_a_device() {
        let app = Router::new()
            .route("/health", get(ok_handler))
            .route_layer(from_fn(forward_device_events));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("x-wylde-events").is_none());
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"reached");
    }

    /// Device present but device_gate unreachable (no pipe in a unit
    /// test) → the layer degrades gracefully: 200, no header, no 5xx.
    /// Proves the middleware reads the `Device` extension and survives a
    /// device_gate outage, matching `stash_pending_events`' best-effort
    /// contract.
    #[tokio::test]
    async fn forward_device_events_degrades_when_device_gate_is_down() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn(forward_device_events));
        let mut request = HttpRequest::builder()
            .uri("/x")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(Device {
            device_id: "dev_A".to_owned(),
            tier: "tool_use".to_owned(),
        });
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get("x-wylde-events").is_none(),
            "a device_gate outage must not produce a stale/partial header"
        );
    }
}
