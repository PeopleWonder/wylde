//! Cross-language parity for the `device_gate.*` action surface.
//!
//! Each test below mirrors a known-good scenario from
//! `device_gate/tests/test_device_gate.py` and asserts that the in-process
//! Rust handlers produce the same reply shape Python would. The corpus is
//! the canonical R2 acceptance gate — when this drifts, the wire shape has
//! diverged and Python clients will see different responses than Rust ones.
//!
//! A follow-up task can spawn the Python service side-by-side and assert
//! byte-equality; for R2 the static expectations are enough to lock the
//! contract, matching the approach R1 (vram-broker) shipped with.

use std::io::Write;
use std::sync::Mutex;

use serde_json::{json, Value};
use tempfile::{NamedTempFile, TempDir};

use wylde_device_gate::core::{install_service, reset_service, DeviceGateService};
use wylde_device_gate::pipe;
use wylde_device_gate::store::DeviceStore;
use wylde_shared::ipc::{dispatch_action, list_actions, register_action, Reply};

// ── Test scaffolding ──────────────────────────────────────────────────

/// Parallel-test guard. Cargo runs integration tests in parallel by default
/// and every case here mutates the singleton service, so they must run
/// serially.
async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    use tokio::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::const_new(());
    LOCK.lock().await
}

fn write_apr1_htpasswd() -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("tempfile");
    // apr1("letmein", salt="abcdefgh") — verified against passlib.
    f.write_all(b"wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n")
        .expect("write");
    f
}

/// Fresh service with tmpdir-backed store + htpasswd. Returns the temp
/// holdings so the caller keeps them alive for the test's lifetime.
struct Harness {
    _tmp: TempDir,
    _htpasswd: NamedTempFile,
}

fn fresh() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let htpasswd = write_apr1_htpasswd();
    let store = DeviceStore::new(tmp.path().join("devices.json"));
    let svc = DeviceGateService::builder()
        .store(store)
        .htpasswd_path(htpasswd.path())
        .build();
    install_service(svc);
    // Reinstall the action surface so handlers point at the fresh singleton.
    pipe::uninstall();
    pipe::install();
    Harness {
        _tmp: tmp,
        _htpasswd: htpasswd,
    }
}

async fn call(action: &str, payload: Value) -> Reply {
    dispatch_action(json!({
        "action": action,
        "payload": payload,
    }))
    .await
}

// One-shot helper: open pairing window + complete it, returning the
// {device_id, token} pair. Most tests need a paired device first.
async fn pair_one() -> (String, String) {
    let started = call("device_gate.start_pairing", json!({})).await;
    assert!(started.ok, "start_pairing failed: {started:?}");
    let code = started.data["code"].as_str().unwrap().to_string();
    let paired = call(
        "device_gate.complete_pairing",
        json!({
            "code": code,
            "username": "wylde",
            "password": "letmein",
        }),
    )
    .await;
    assert!(paired.ok, "complete_pairing failed: {paired:?}");
    (
        paired.data["device_id"].as_str().unwrap().to_string(),
        paired.data["token"].as_str().unwrap().to_string(),
    )
}

// ── Pairing flow ──────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_start_pairing_returns_code_and_expiry() {
    let _g = test_guard().await;
    let _h = fresh();
    let r = call("device_gate.start_pairing", json!({})).await;
    assert!(r.ok);
    assert!(r.data["code"].is_string());
    assert_eq!(r.data["code"].as_str().unwrap().len(), 6);
    assert!(r.data["expires_at"].as_f64().unwrap() > 0.0);
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_get_pairing_status_after_start() {
    let _g = test_guard().await;
    let _h = fresh();
    call("device_gate.start_pairing", json!({})).await;
    let r = call("device_gate.get_pairing_status", json!({})).await;
    assert!(r.ok);
    assert_eq!(r.data["pairing_active"], true);
    assert!(r.data["code"].is_string());
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_cancel_returns_cancelled_flag() {
    let _g = test_guard().await;
    let _h = fresh();
    call("device_gate.start_pairing", json!({})).await;
    let r = call("device_gate.cancel_pairing", json!({})).await;
    assert!(r.ok);
    assert_eq!(r.data["ok"], true);
    assert_eq!(r.data["cancelled"], true);
    // Cancel-when-off is a benign no-op.
    let r = call("device_gate.cancel_pairing", json!({})).await;
    assert_eq!(r.data["cancelled"], false);
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_complete_pairing_happy_path() {
    let _g = test_guard().await;
    let _h = fresh();
    let started = call("device_gate.start_pairing", json!({})).await;
    let code = started.data["code"].as_str().unwrap().to_string();
    let r = call(
        "device_gate.complete_pairing",
        json!({
            "code": code,
            "username": "wylde",
            "password": "letmein",
            "device_metadata": {"name": "iPhone-15"},
        }),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert!(r.data["device_id"].is_string());
    assert!(r.data["token"].is_string());
    assert_eq!(r.data["tier"], "read_only");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_complete_pairing_wrong_code() {
    let _g = test_guard().await;
    let _h = fresh();
    call("device_gate.start_pairing", json!({})).await;
    let r = call(
        "device_gate.complete_pairing",
        json!({
            "code": "000000",
            "username": "wylde",
            "password": "letmein",
        }),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.error.as_ref().unwrap().code, "code_mismatch");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_complete_pairing_wrong_credentials() {
    let _g = test_guard().await;
    let _h = fresh();
    let started = call("device_gate.start_pairing", json!({})).await;
    let code = started.data["code"].as_str().unwrap().to_string();
    let r = call(
        "device_gate.complete_pairing",
        json!({
            "code": code,
            "username": "wylde",
            "password": "WRONG",
        }),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.error.as_ref().unwrap().code, "credential_mismatch");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_complete_pairing_without_window() {
    let _g = test_guard().await;
    let _h = fresh();
    let r = call(
        "device_gate.complete_pairing",
        json!({
            "code": "123456",
            "username": "wylde",
            "password": "letmein",
        }),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.error.as_ref().unwrap().code, "pairing_inactive");
    reset_service();
}

// ── Verify ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_verify_returns_device_id_and_tier() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, token) = pair_one().await;
    let r = call("device_gate.verify", json!({ "token": token })).await;
    assert!(r.ok);
    assert_eq!(r.data["device_id"], did);
    assert_eq!(r.data["tier"], "read_only");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_verify_rejects_invalid_token() {
    let _g = test_guard().await;
    let _h = fresh();
    let r = call("device_gate.verify", json!({ "token": "not-a-real-token" })).await;
    assert!(!r.ok);
    assert_eq!(r.error.as_ref().unwrap().code, "invalid_token");
    reset_service();
}

// ── Tier management ───────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_set_tier_persists() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, token) = pair_one().await;
    let r = call(
        "device_gate.set_tier",
        json!({ "device_id": did, "tier": "tool_use" }),
    )
    .await;
    assert!(r.ok);
    assert_eq!(r.data["tier"], "tool_use");
    // Verify reflects the new tier.
    let v = call("device_gate.verify", json!({ "token": token })).await;
    assert_eq!(v.data["tier"], "tool_use");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_set_tier_rejects_unknown() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, _t) = pair_one().await;
    let r = call(
        "device_gate.set_tier",
        json!({ "device_id": did, "tier": "superuser" }),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.error.as_ref().unwrap().code, "bad_request");
    reset_service();
}

// ── Token rotation ────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_rotate_invalidates_old_and_returns_new() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, old_token) = pair_one().await;
    let r = call("device_gate.rotate_token", json!({ "device_id": did })).await;
    assert!(r.ok);
    let new_token = r.data["new_token"].as_str().unwrap().to_string();
    assert_ne!(new_token, old_token);

    // New token works.
    let v = call("device_gate.verify", json!({ "token": new_token })).await;
    assert!(v.ok);
    // Old token rejected.
    let v = call("device_gate.verify", json!({ "token": old_token })).await;
    assert!(!v.ok);
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_rotate_emits_token_rotated_event() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, token) = pair_one().await;
    // Active session.
    call("device_gate.verify", json!({ "token": token })).await;
    let r = call("device_gate.rotate_token", json!({ "device_id": did })).await;
    let new_token = r.data["new_token"].as_str().unwrap().to_string();

    let events = call(
        "device_gate.consume_pending_events",
        json!({ "device_id": did }),
    )
    .await;
    assert!(events.ok);
    let arr = events.data["events"].as_array().unwrap();
    let rotation = arr.iter().find(|e| e["type"] == "token_rotated").unwrap();
    assert_eq!(rotation["new_token"], new_token);
    reset_service();
}

// ── Revocation ────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_revoke_removes_device() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, token) = pair_one().await;
    let r = call("device_gate.revoke", json!({ "device_id": did })).await;
    assert!(r.ok);
    assert_eq!(r.data["device_id"], did);

    let list = call("device_gate.list_devices", json!({})).await;
    assert_eq!(list.data["count"], 0);

    let v = call("device_gate.verify", json!({ "token": token })).await;
    assert!(!v.ok);
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_revoke_unknown_is_not_found() {
    let _g = test_guard().await;
    let _h = fresh();
    let r = call(
        "device_gate.revoke",
        json!({ "device_id": "dev_nonexistent" }),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.error.as_ref().unwrap().code, "not_found");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_revoke_emits_event() {
    let _g = test_guard().await;
    let _h = fresh();
    let (did, _t) = pair_one().await;
    call("device_gate.revoke", json!({ "device_id": did.clone() })).await;
    let events = call(
        "device_gate.consume_pending_events",
        json!({ "device_id": did }),
    )
    .await;
    assert!(events.data["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["type"] == "revoked"));
    reset_service();
}

// ── Listing ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_list_devices_returns_count_and_array() {
    let _g = test_guard().await;
    let _h = fresh();
    pair_one().await;
    let r = call("device_gate.list_devices", json!({})).await;
    assert!(r.ok);
    assert_eq!(r.data["count"], 1);
    let arr = r.data["devices"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    // Public list must NOT include the token.
    assert!(
        arr[0].get("token").is_none(),
        "list_devices must not expose tokens"
    );
    // Tier + standard fields present.
    for field in [
        "device_id",
        "name",
        "tier",
        "paired_at",
        "last_seen",
        "metadata",
        "is_active",
    ] {
        assert!(
            arr[0].get(field).is_some(),
            "list_devices missing field {field}"
        );
    }
    reset_service();
}

// ── recent_actions (per-device audit strip) ───────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_recent_actions_orders_pair_tier_rotate_newest_first() {
    let _g = test_guard().await;
    let _h = fresh();
    let (device_id, _token) = pair_one().await;

    let st = call(
        "device_gate.set_tier",
        json!({"device_id": device_id, "tier": "tool_use"}),
    )
    .await;
    assert!(st.ok, "set_tier failed: {st:?}");
    let rot = call("device_gate.rotate_token", json!({"device_id": device_id})).await;
    assert!(rot.ok, "rotate failed: {rot:?}");

    let r = call(
        "device_gate.recent_actions",
        json!({"device_id": device_id, "limit": 20}),
    )
    .await;
    assert!(r.ok);
    assert_eq!(r.data["device_id"], device_id);
    assert_eq!(r.data["count"], 3);
    let actions = r.data["actions"].as_array().unwrap();
    // Newest-first: rotate, tier, paired — matches Python's ActionLog.recent.
    assert_eq!(actions[0]["action"], "token rotated");
    assert_eq!(actions[1]["action"], "tier → tool_use");
    assert_eq!(actions[2]["action"], "paired");
    assert_eq!(actions[0]["status"], "ok");
    // ISO-8601 UTC second-resolution timestamp.
    let ts = actions[0]["timestamp"].as_str().unwrap();
    assert!(ts.ends_with('Z') && ts.len() == 20, "bad timestamp {ts}");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_recent_actions_survives_revoke() {
    let _g = test_guard().await;
    let _h = fresh();
    let (device_id, _token) = pair_one().await;
    let rev = call("device_gate.revoke", json!({"device_id": device_id})).await;
    assert!(rev.ok, "revoke failed: {rev:?}");
    // Device row is gone …
    assert_eq!(
        call("device_gate.list_devices", json!({})).await.data["count"],
        0
    );
    // … but the audit trail (paired + revoked) is preserved.
    let r = call(
        "device_gate.recent_actions",
        json!({"device_id": device_id}),
    )
    .await;
    assert!(r.ok);
    assert_eq!(r.data["count"], 2);
    assert_eq!(r.data["actions"][0]["action"], "revoked");
    assert_eq!(r.data["actions"][1]["action"], "paired");
    reset_service();
}

#[tokio::test(flavor = "current_thread")]
async fn parity_recent_actions_unknown_device_empty() {
    let _g = test_guard().await;
    let _h = fresh();
    let r = call(
        "device_gate.recent_actions",
        json!({"device_id": "dev_unknown"}),
    )
    .await;
    assert!(r.ok);
    assert_eq!(r.data["count"], 0);
    assert!(r.data["actions"].as_array().unwrap().is_empty());
    reset_service();
}

// ── Surface enumeration — guards against accidental rename / drop ─────

#[tokio::test(flavor = "current_thread")]
async fn parity_action_surface_is_complete() {
    let _g = test_guard().await;
    let _h = fresh();
    // Touch a dummy action so the registry isn't empty.
    register_action("test.ping", |_v: Value| async {
        wylde_shared::ipc::Reply::ok(Value::Null)
    });
    let registered = list_actions();
    for expected in [
        "device_gate.list_devices",
        "device_gate.start_pairing",
        "device_gate.cancel_pairing",
        "device_gate.get_pairing_status",
        "device_gate.complete_pairing",
        "device_gate.verify",
        "device_gate.set_tier",
        "device_gate.rotate_token",
        "device_gate.revoke",
        "device_gate.recent_actions",
        "device_gate.consume_pending_events",
    ] {
        assert!(
            registered.iter().any(|n| n == expected),
            "expected action {expected} not in registered set"
        );
    }
    reset_service();
}

// ── Envelope contract ─────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parity_bad_request_for_missing_required_fields() {
    let _g = test_guard().await;
    let _h = fresh();
    // verify without token, set_tier without device_id, etc. — all bad_request.
    for (action, payload) in [
        ("device_gate.verify", json!({})),
        ("device_gate.set_tier", json!({"device_id": "x"})),
        ("device_gate.rotate_token", json!({})),
        ("device_gate.revoke", json!({})),
        ("device_gate.recent_actions", json!({})),
        ("device_gate.consume_pending_events", json!({})),
    ] {
        let r = call(action, payload).await;
        assert!(!r.ok, "{action} should have failed");
        assert_eq!(
            r.error.as_ref().unwrap().code,
            "bad_request",
            "{action} wrong error code"
        );
    }
    reset_service();
}

/// Tracks down a regression-class bug: the parity scaffolding must clear
/// the singleton between tests. If `reset_service()` is skipped, later
/// tests inherit pairings from earlier ones. This test confirms the
/// scaffolding works.
#[tokio::test(flavor = "current_thread")]
async fn scaffolding_resets_state_between_calls() {
    let _g = test_guard().await;
    let _h = fresh();
    pair_one().await;
    assert_eq!(
        call("device_gate.list_devices", json!({})).await.data["count"],
        1
    );
    reset_service();
    let _h2 = fresh();
    assert_eq!(
        call("device_gate.list_devices", json!({})).await.data["count"],
        0
    );
    reset_service();
}

// Silence unused-import for the `Mutex` from std (kept for future fixtures).
#[allow(dead_code)]
fn _unused_marker() -> Mutex<()> {
    Mutex::new(())
}
