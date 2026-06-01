//! device_gate pipe — `\\.\pipe\wylde-device-gate`.
//!
//! Rust port of `device_gate/pipe.py`. Ten `device_gate.*` actions backed by
//! [`crate::core`]. The GUI drives pairing / tier / rotate / revoke; the
//! Gateway calls `device_gate.verify` and `device_gate.consume_pending_events`
//! on every authenticated request.
//!
//! Same envelope contract every Wylde service uses: handlers take the JSON
//! payload and return a [`Reply`]. Errors land as
//! `{ok: false, error: {code, message}}` on the wire so Python and Rust
//! callers see the same shape.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, unregister_action, IpcError, Reply};

use crate::core::{with_service, DeviceGateError};

pub const SERVICE_NAME: &str = "wylde-device-gate";

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Action surface. Matches `device_gate/pipe.py::_ACTIONS` one-to-one.
const ACTION_NAMES: [&str; 10] = [
    "device_gate.list_devices",
    "device_gate.start_pairing",
    "device_gate.cancel_pairing",
    "device_gate.get_pairing_status",
    "device_gate.complete_pairing",
    "device_gate.verify",
    "device_gate.set_tier",
    "device_gate.rotate_token",
    "device_gate.revoke",
    "device_gate.consume_pending_events",
];

/// Register every `device_gate.*` action on the process-wide pipe registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    register_action_with_meta(
        "device_gate.list_devices",
        |p: Value| async move { handle_list_devices(p).await },
        "List all paired devices with tier + last-seen.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.start_pairing",
        |p: Value| async move { handle_start_pairing(p).await },
        "Open a pairing window; returns {code, expires_at}.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.cancel_pairing",
        |p: Value| async move { handle_cancel_pairing(p).await },
        "Cancel any active pairing window.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.get_pairing_status",
        |p: Value| async move { handle_get_pairing_status(p).await },
        "Return current pairing-mode state.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.complete_pairing",
        |p: Value| async move { handle_complete_pairing(p).await },
        "Finish pairing with code + credentials; returns {device_id, token, tier}.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.verify",
        |p: Value| async move { handle_verify(p).await },
        "Look up device by token; touches last_seen.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.set_tier",
        |p: Value| async move { handle_set_tier(p).await },
        "Change a device's permission tier.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.rotate_token",
        |p: Value| async move { handle_rotate_token(p).await },
        "Mint a new token for a device; old one invalidated immediately.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.revoke",
        |p: Value| async move { handle_revoke(p).await },
        "Remove a device; its token is invalidated.",
        "wylde_device_gate::pipe",
    );
    register_action_with_meta(
        "device_gate.consume_pending_events",
        |p: Value| async move { handle_consume_pending_events(p).await },
        "Gateway-only: drain queued events (tier_changed, token_rotated, revoked) for a device.",
        "wylde_device_gate::pipe",
    );
    tracing::info!(
        "device_gate pipe: registered {} device_gate.* actions",
        ACTION_NAMES.len()
    );
}

/// Test-only: drop every action handler. Mirrors `service::reset_for_tests`
/// in the R1 broker.
pub fn uninstall() {
    for name in ACTION_NAMES {
        unregister_action(name);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

// ── Payload helpers ────────────────────────────────────────────────────

fn payload_dict(payload: Value) -> Result<serde_json::Map<String, Value>, IpcError> {
    match payload {
        Value::Null => Ok(serde_json::Map::new()),
        Value::Object(m) => Ok(m),
        _ => Err(IpcError::new("bad_request", "payload must be a map")),
    }
}

fn require_str(map: &serde_json::Map<String, Value>, key: &str) -> Result<String, IpcError> {
    match map.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(IpcError::new("bad_request", format!("{key} is required"))),
    }
}

fn err_to_ipc(e: DeviceGateError) -> IpcError {
    IpcError::new(e.code, e.message)
}

// ── Handlers ──────────────────────────────────────────────────────────

async fn handle_list_devices(_payload: Value) -> Reply {
    let devices = with_service(|svc| svc.list_devices(60.0));
    Reply::ok(json!({
        "devices": devices,
        "count": devices.len(),
    }))
}

async fn handle_start_pairing(_payload: Value) -> Reply {
    Reply::ok(with_service(|svc| svc.start_pairing()))
}

async fn handle_cancel_pairing(_payload: Value) -> Reply {
    Reply::ok(with_service(|svc| svc.cancel_pairing()))
}

async fn handle_get_pairing_status(_payload: Value) -> Reply {
    Reply::ok(with_service(|svc| svc.get_pairing_status()))
}

async fn handle_complete_pairing(payload: Value) -> Reply {
    let map = match payload_dict(payload) {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let code = match require_str(&map, "code") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    let username = match require_str(&map, "username") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    let password = match require_str(&map, "password") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    let metadata: HashMap<String, Value> = match map.get("device_metadata") {
        None | Some(Value::Null) => HashMap::new(),
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Some(_) => {
            return Reply::err(IpcError::new(
                "bad_request",
                "device_metadata must be a map",
            ))
        }
    };
    match with_service(|svc| svc.complete_pairing(&code, &username, &password, Some(&metadata))) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(err_to_ipc(e)),
    }
}

async fn handle_verify(payload: Value) -> Reply {
    let map = match payload_dict(payload) {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let token = match require_str(&map, "token") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    match with_service(|svc| svc.verify(&token)) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(err_to_ipc(e)),
    }
}

async fn handle_set_tier(payload: Value) -> Reply {
    let map = match payload_dict(payload) {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let device_id = match require_str(&map, "device_id") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    let tier = match require_str(&map, "tier") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    match with_service(|svc| svc.set_tier(&device_id, &tier)) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(err_to_ipc(e)),
    }
}

async fn handle_rotate_token(payload: Value) -> Reply {
    let map = match payload_dict(payload) {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let device_id = match require_str(&map, "device_id") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    match with_service(|svc| svc.rotate_token(&device_id)) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(err_to_ipc(e)),
    }
}

async fn handle_revoke(payload: Value) -> Reply {
    let map = match payload_dict(payload) {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let device_id = match require_str(&map, "device_id") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    match with_service(|svc| svc.revoke(&device_id)) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(err_to_ipc(e)),
    }
}

async fn handle_consume_pending_events(payload: Value) -> Reply {
    let map = match payload_dict(payload) {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let device_id = match require_str(&map, "device_id") {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    let events = with_service(|svc| svc.consume_pending_events(&device_id));
    Reply::ok(json!({
        "events": events,
        "count": events.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{install_service, reset_service, DeviceGateService};
    use crate::store::DeviceStore;
    use crate::test_lock::guard;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn write_apr1_htpasswd() -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(b"wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n")
            .expect("write");
        f
    }

    fn install_fresh() -> (TempDir, NamedTempFile) {
        let tmp = TempDir::new().expect("tempdir");
        let htpasswd = write_apr1_htpasswd();
        let store = DeviceStore::new(tmp.path().join("devices.json"));
        let svc = DeviceGateService::builder()
            .store(store)
            .htpasswd_path(htpasswd.path())
            .build();
        install_service(svc);
        uninstall();
        install();
        (tmp, htpasswd)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_devices_empty() {
        let _g = guard().await;
        let (_t, _h) = install_fresh();
        let reply = handle_list_devices(Value::Null).await;
        assert!(reply.ok);
        assert_eq!(reply.data["count"], 0);
        reset_service();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_then_cancel_pairing() {
        let _g = guard().await;
        let (_t, _h) = install_fresh();
        let r = handle_start_pairing(Value::Null).await;
        assert!(r.ok);
        assert!(r.data["code"].as_str().unwrap().len() == 6);
        let r = handle_cancel_pairing(Value::Null).await;
        assert_eq!(r.data["cancelled"], true);
        reset_service();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_missing_token_is_bad_request() {
        let _g = guard().await;
        let (_t, _h) = install_fresh();
        let r = handle_verify(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
        reset_service();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_pairing_requires_all_fields() {
        let _g = guard().await;
        let (_t, _h) = install_fresh();
        for missing in &["code", "username", "password"] {
            let mut payload = serde_json::Map::new();
            for field in &["code", "username", "password"] {
                if field != missing {
                    payload.insert((*field).into(), Value::String("x".into()));
                }
            }
            let r = handle_complete_pairing(Value::Object(payload)).await;
            assert!(!r.ok, "missing {missing} should fail");
            assert_eq!(r.error.unwrap().code, "bad_request");
        }
        reset_service();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_pairing_metadata_must_be_map() {
        let _g = guard().await;
        let (_t, _h) = install_fresh();
        handle_start_pairing(Value::Null).await;
        let r = handle_complete_pairing(json!({
            "code": "123456",
            "username": "wylde",
            "password": "letmein",
            "device_metadata": "not-a-map",
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
        reset_service();
    }
}
