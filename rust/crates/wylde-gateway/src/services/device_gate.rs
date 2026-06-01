//! Thin wrapper around `\\.\pipe\wylde-device-gate`.
//!
//! Rust port of `Gateway/services/device_gate.py`. Gateway routes call
//! into here; the wrapper translates each public function into a pipe
//! action call via [`wylde_shared::ipc::call_action`]. This keeps the
//! route handlers free of pipe-envelope plumbing.
//!
//! Every call returns `(http_status, body)` so the caller can hand the
//! tuple straight back to a [`crate::envelopes::failure`] / `success`
//! response builder. Wave 1 exposed [`verify`] only; wave 2d adds the
//! remaining `device_gate.*` actions for the `routes::devices` +
//! `routes::link` ports — `list_devices`, `start_pairing`,
//! `cancel_pairing`, `get_pairing_status`, `complete_pairing`,
//! `set_tier`, `rotate_token`, `revoke`, `consume_pending_events`.

use axum::http::StatusCode;
use serde_json::{json, Value};
use wylde_shared::ipc::{call_action, IpcError};

/// `\\.\pipe\wylde-device-gate` service name.
pub const SERVICE_NAME: &str = "wylde-device-gate";

/// Result type returned by every wrapper. `Ok` carries the successful
/// payload; `Err` carries an `(http_status, error_body)` tuple already
/// shaped for the JSON envelope.
pub type GateResult = Result<Value, (StatusCode, Value)>;

/// Look up the device that owns `token`. Touches its `last_seen`.
///
/// Used by the future auth layer to translate the
/// `Authorization: Bearer <token>` header into a device tier.
pub async fn verify(token: &str) -> GateResult {
    call("device_gate.verify", json!({"token": token})).await
}

/// `device_gate.list_devices` — return every registered device.
pub async fn list_devices() -> GateResult {
    call("device_gate.list_devices", Value::Null).await
}

/// `device_gate.start_pairing` — open a fresh pairing window.
pub async fn start_pairing() -> GateResult {
    call("device_gate.start_pairing", Value::Null).await
}

/// `device_gate.cancel_pairing` — close the current pairing window.
pub async fn cancel_pairing() -> GateResult {
    call("device_gate.cancel_pairing", Value::Null).await
}

/// `device_gate.get_pairing_status` — report whether a pairing window
/// is active and how much time remains.
pub async fn get_pairing_status() -> GateResult {
    call("device_gate.get_pairing_status", Value::Null).await
}

/// `device_gate.complete_pairing` — exchange the one-time pairing code
/// plus admin credentials for a fresh device token. Mobile uses this
/// to finish the link flow; the returned token then powers every
/// subsequent Bearer-authed request.
pub async fn complete_pairing(
    code: &str,
    username: &str,
    password: &str,
    device_metadata: Value,
) -> GateResult {
    call(
        "device_gate.complete_pairing",
        json!({
            "code": code,
            "username": username,
            "password": password,
            "device_metadata": device_metadata,
        }),
    )
    .await
}

/// `device_gate.set_tier` — change a device's tier (e.g. demote a lost
/// phone to `revoked`).
pub async fn set_tier(device_id: &str, tier: &str) -> GateResult {
    call(
        "device_gate.set_tier",
        json!({"device_id": device_id, "tier": tier}),
    )
    .await
}

/// `device_gate.rotate_token` — mint a fresh token for an existing
/// device, invalidating the old one.
pub async fn rotate_token(device_id: &str) -> GateResult {
    call("device_gate.rotate_token", json!({"device_id": device_id})).await
}

/// `device_gate.revoke` — remove the device entirely.
pub async fn revoke(device_id: &str) -> GateResult {
    call("device_gate.revoke", json!({"device_id": device_id})).await
}

/// `device_gate.consume_pending_events` — drain queued events for a
/// device. The `/api/devices/me` route uses this so mobile can poll
/// without triggering a chat turn.
pub async fn consume_pending_events(device_id: &str) -> GateResult {
    call(
        "device_gate.consume_pending_events",
        json!({"device_id": device_id}),
    )
    .await
}

async fn call(action: &str, payload: Value) -> GateResult {
    match call_action(SERVICE_NAME, action, payload).await {
        Ok(data) => Ok(data),
        Err(err) => Err((err_code_to_http(&err), to_envelope(&err))),
    }
}

fn err_code_to_http(err: &IpcError) -> StatusCode {
    let code = err.code.to_ascii_lowercase();
    let msg = err.message.to_ascii_lowercase();
    if code == "not_found" || msg.contains("not found") {
        return StatusCode::NOT_FOUND;
    }
    if matches!(
        code.as_str(),
        "bad_request" | "invalid_token" | "code_mismatch" | "credential_mismatch"
    ) {
        return StatusCode::BAD_REQUEST;
    }
    if code == "pairing_inactive" {
        return StatusCode::CONFLICT;
    }
    // String-tag fallbacks for legacy Python error messages that wrap the
    // canonical code in `[brackets]`.
    if msg.contains("[invalid_token]") || msg.contains("[credential_mismatch]") {
        return StatusCode::BAD_REQUEST;
    }
    if msg.contains("[not_found]") {
        return StatusCode::NOT_FOUND;
    }
    if msg.contains("[pairing_inactive]") {
        return StatusCode::CONFLICT;
    }
    StatusCode::BAD_GATEWAY
}

fn to_envelope(err: &IpcError) -> Value {
    let message = if err.message.is_empty() {
        "device-gate call failed".to_owned()
    } else {
        err.message.clone()
    };
    json!({
        "ok": false,
        "error": {
            "code": err.code,
            "message": message,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_code_maps_not_found_to_404() {
        let e = IpcError::new("not_found", "");
        assert_eq!(err_code_to_http(&e), StatusCode::NOT_FOUND);
    }

    #[test]
    fn err_code_maps_invalid_token_to_400() {
        let e = IpcError::new("invalid_token", "token expired");
        assert_eq!(err_code_to_http(&e), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn err_code_maps_pairing_inactive_to_409() {
        let e = IpcError::new("pairing_inactive", "no active window");
        assert_eq!(err_code_to_http(&e), StatusCode::CONFLICT);
    }

    #[test]
    fn err_code_falls_back_to_502() {
        let e = IpcError::new("transport", "pipe disconnected");
        assert_eq!(err_code_to_http(&e), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn err_code_picks_up_bracketed_legacy_message() {
        let e = IpcError::new("unknown", "request rejected [invalid_token]");
        assert_eq!(err_code_to_http(&e), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn envelope_carries_code_and_message() {
        let e = IpcError::new("invalid_token", "expired at 2026-01-01");
        let env = to_envelope(&e);
        assert_eq!(env["ok"], false);
        assert_eq!(env["error"]["code"], "invalid_token");
        assert_eq!(env["error"]["message"], "expired at 2026-01-01");
    }

    #[test]
    fn envelope_defaults_empty_message() {
        let e = IpcError::new("transport", "");
        let env = to_envelope(&e);
        assert_eq!(env["error"]["message"], "device-gate call failed");
    }
}
