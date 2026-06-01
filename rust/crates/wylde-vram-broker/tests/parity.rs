//! Cross-language parity for the action surface.
//!
//! For each Python test case in `Core/resource_monitor/test_vram_broker.py`
//! we replay the equivalent action against the in-process Rust handlers and
//! assert the reply shape matches what the Python broker would return.
//!
//! This is a fixture-driven test: each entry below has a payload and a list
//! of expectation closures applied to the [`Reply`]. The corpus is the
//! canonical R1 acceptance gate — when this drifts, the wire shape has
//! diverged and Python clients will see different responses than Rust ones.
//!
//! A follow-up task (tracked separately) will spawn the Python broker
//! side-by-side in CI and assert byte-equality. For R1 the static
//! expectations are enough to lock the contract.

use serde_json::{json, Value};
use wylde_shared::ipc::{dispatch_action, register_action};
use wylde_vram_broker::registry::registry;
use wylde_vram_broker::service::{install, reset_for_tests};

const GB: u64 = 1024 * 1024 * 1024;

fn fresh_broker() {
    reset_for_tests();
    registry().set_gpu(16 * GB, 0, "TestGPU");
    install(false);
}

async fn call(action: &str, payload: Value) -> wylde_shared::ipc::Reply {
    dispatch_action(json!({
        "action": action,
        "payload": payload,
    }))
    .await
}

/// Parallel-test guard. Cargo runs integration tests in parallel by default
/// and every case here mutates the singleton registry, so they must run
/// serially. Uses a tokio async-aware Mutex so the guard can be held across
/// `.await` (the `await_holding_lock` clippy lint forbids that with
/// `std::sync::Mutex`).
async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    use tokio::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::const_new(());
    LOCK.lock().await
}

#[tokio::test(flavor = "current_thread")]
async fn parity_reserve_grants_when_fits() {
    let _g = test_guard().await;
    fresh_broker();
    // Mirror Python test_reserve_grants_when_fits.
    let reply = call(
        "vram.reserve",
        json!({
            "service": "wylde-caption",
            "model": "florence-2",
            "bytes": 4 * GB,
            "priority": 40,
            "ttl": 60,
        }),
    )
    .await;
    assert!(reply.ok, "reply not ok: {reply:?}");
    assert_eq!(reply.data["service"], "wylde-caption");
    assert_eq!(reply.data["bytes"], 4 * GB);
    assert_eq!(reply.data["priority"], 40);
    assert!(reply.data["lease_id"].is_string());
    assert_eq!(reply.data["synthetic"], false);
}

#[tokio::test(flavor = "current_thread")]
async fn parity_reserve_rejects_total_too_small() {
    let _g = test_guard().await;
    fresh_broker();
    // Mirror Python test_reserve_rejects_when_total_too_small.
    let reply = call(
        "vram.reserve",
        json!({
            "service": "wylde-trainer",
            "model": "big-llm",
            "bytes": 20 * GB,
            "priority": 20,
        }),
    )
    .await;
    assert!(!reply.ok);
    assert_eq!(reply.error.as_ref().unwrap().code, "would_exceed_total");
}

#[tokio::test(flavor = "current_thread")]
async fn parity_reserve_rejects_no_headroom_no_preempt() {
    let _g = test_guard().await;
    fresh_broker();
    let _ = call(
        "vram.reserve",
        json!({
            "service": "ollama",
            "model": "gemma3-27b",
            "bytes": 13 * GB,
            "priority": 100,
        }),
    )
    .await;
    let reply = call(
        "vram.reserve",
        json!({
            "service": "wylde-trainer",
            "model": "lora",
            "bytes": 4 * GB,
            "priority": 20,
        }),
    )
    .await;
    assert!(!reply.ok);
    let err = reply.error.as_ref().unwrap();
    assert_eq!(err.code, "insufficient_vram");
    let details = err.details.as_ref().expect("details on insufficient_vram");
    assert_eq!(details["requested_bytes"], 4 * GB);
    let blockers = details["blockers"].as_array().expect("blockers array");
    assert!(blockers.iter().any(|b| b["service"] == "ollama"));
}

#[tokio::test(flavor = "current_thread")]
async fn parity_release_frees_bytes() {
    let _g = test_guard().await;
    fresh_broker();
    let granted = call(
        "vram.reserve",
        json!({
            "service": "wylde-rag",  // wylde-check: dead-ref-ok
            "model": "reranker",
            "bytes": 2 * GB,
            "priority": 60,
        }),
    )
    .await;
    let lid = granted.data["lease_id"].as_str().unwrap().to_owned();

    let r1 = call("vram.release", json!({"lease_id": lid.clone()})).await;
    assert!(r1.ok);
    assert_eq!(r1.data["known"], true);
    assert_eq!(r1.data["freed_bytes"], 2 * GB);

    // Re-release is idempotent.
    let r2 = call("vram.release", json!({"lease_id": lid})).await;
    assert_eq!(r2.data["known"], false);
}

#[tokio::test(flavor = "current_thread")]
async fn parity_heartbeat_extends_ttl() {
    let _g = test_guard().await;
    fresh_broker();
    let granted = call(
        "vram.reserve",
        json!({
            "service": "wylde-caption",
            "model": "x",
            "bytes": GB,
            "priority": 40,
            "ttl": 5,
        }),
    )
    .await;
    let lid = granted.data["lease_id"].as_str().unwrap().to_owned();
    let first_exp = granted.data["expires_at"].as_f64().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));
    let hb = call("vram.heartbeat", json!({"lease_id": lid, "ttl": 60})).await;
    assert!(hb.ok);
    assert!(hb.data["expires_at"].as_f64().unwrap() > first_exp);
}

#[tokio::test(flavor = "current_thread")]
async fn parity_heartbeat_unknown_returns_not_found() {
    let _g = test_guard().await;
    fresh_broker();
    let reply = call("vram.heartbeat", json!({"lease_id": "nope"})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.as_ref().unwrap().code, "not_found");
}

#[tokio::test(flavor = "current_thread")]
async fn parity_nonce_dedupes() {
    let _g = test_guard().await;
    fresh_broker();
    let payload = json!({
        "service": "wylde-caption",
        "model": "x",
        "bytes": GB,
        "priority": 40,
        "client_nonce": "abc123",
    });
    let r1 = call("vram.reserve", payload.clone()).await;
    let r2 = call("vram.reserve", payload).await;
    assert_eq!(r1.data["lease_id"], r2.data["lease_id"]);
    assert_eq!(registry().reserved_total(), GB);
}

#[tokio::test(flavor = "current_thread")]
async fn parity_state_shape() {
    let _g = test_guard().await;
    fresh_broker();
    let _ = call(
        "vram.reserve",
        json!({
            "service": "wylde-caption",
            "model": "x",
            "bytes": 2 * GB,
            "priority": 40,
        }),
    )
    .await;
    let s = call("vram.state", json!({})).await;
    assert!(s.ok);
    assert_eq!(s.data["gpu"]["total_bytes"], 16 * GB);
    assert_eq!(s.data["gpu"]["reserved_bytes"], 2 * GB);
    let max_free: u64 = (16 - 2) * GB;
    assert!(s.data["gpu"]["free_for_grant"].as_u64().unwrap() <= max_free);
    let leases = s.data["leases"].as_array().unwrap();
    assert!(leases.iter().any(|l| l["service"] == "wylde-caption"));
    let priorities: Vec<i64> = s.data["by_service"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["priority"].as_i64().unwrap())
        .collect();
    let mut sorted = priorities.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(priorities, sorted);
}

#[tokio::test(flavor = "current_thread")]
async fn parity_leases_endpoint_returns_wire_shape() {
    let _g = test_guard().await;
    fresh_broker();
    let _ = call(
        "vram.reserve",
        json!({
            "service": "wylde-voice",
            "model": "whisper",
            "bytes": 3 * GB,
            "priority": 80,
        }),
    )
    .await;
    let reply = call("vram.leases", json!({})).await;
    assert!(reply.ok);
    let leases = reply.data["leases"].as_array().expect("leases array");
    assert_eq!(leases.len(), 1);
    let lease = &leases[0];
    // Field set must match Python's Lease.to_wire().
    for key in [
        "lease_id",
        "service",
        "model",
        "bytes",
        "priority",
        "granted_at",
        "expires_at",
        "heartbeat_at",
        "pid",
        "synthetic",
        "client_nonce",
    ] {
        assert!(
            lease.get(key).is_some(),
            "lease wire shape missing field {key}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn parity_cache_endpoint_warm_for_decays() {
    let _g = test_guard().await;
    fresh_broker();
    let _ = call(
        "vram.reserve",
        json!({
            "service": "wylde-caption",
            "model": "qwen-vl",
            "bytes": GB,
            "priority": 40,
        }),
    )
    .await;
    let reply = call("vram.cache", json!({})).await;
    assert!(reply.ok);
    assert!(reply.data["ttl_s"].as_f64().unwrap() > 0.0);
    let entries = reply.data["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e["service"], "wylde-caption");
    assert_eq!(e["model"], "qwen-vl");
    assert!(e["warm_for"].as_f64().unwrap() > 0.0);
}

#[tokio::test(flavor = "current_thread")]
async fn parity_evict_unknown_lease_is_not_found() {
    let _g = test_guard().await;
    fresh_broker();
    let reply = call("vram.evict", json!({"lease_id": "nope"})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.as_ref().unwrap().code, "not_found");
}

#[tokio::test(flavor = "current_thread")]
async fn parity_preemption_evicts_lower_priority() {
    let _g = test_guard().await;
    fresh_broker();
    // Mirror Python test_preemption_evicts_lower_priority: a low-priority
    // caption lease holds 10 GB; voice (priority 80) reserves 8 GB with
    // preempt=true. With a well-behaved owner (we simulate by removing the
    // lease from a background task), the preempt path should succeed.
    let granted = call(
        "vram.reserve",
        json!({
            "service": "wylde-caption",
            "model": "qwen-vl",
            "bytes": 10 * GB,
            "priority": 40,
        }),
    )
    .await;
    let caption_lease = granted.data["lease_id"].as_str().unwrap().to_owned();
    let _bg = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        registry().remove(&caption_lease);
    });

    // Block the signal_evict pipe call by registering a no-op action on the
    // caller-loopback isn't worth it — the soft-evict / hard-evict signals
    // will just transport-error and the loop will observe the lease being
    // removed by the background task.
    let reply = call(
        "vram.reserve",
        json!({
            "service": "wylde-voice",
            "model": "whisper-large",
            "bytes": 8 * GB,
            "priority": 80,
            "preempt": true,
        }),
    )
    .await;
    assert!(reply.ok, "preempt should grant: {reply:?}");
    assert_eq!(reply.data["service"], "wylde-voice");
}

/// Action surface enumeration — guards against accidental rename / drop.
#[tokio::test(flavor = "current_thread")]
async fn parity_action_surface_is_complete() {
    let _g = test_guard().await;
    fresh_broker();
    // Touch a dummy action so the registry isn't empty for the listing.
    register_action("test.ping", |_v: Value| async {
        wylde_shared::ipc::Reply::ok(Value::Null)
    });
    let registered = wylde_shared::ipc::list_actions();
    for expected in [
        "vram.reserve",
        "vram.release",
        "vram.heartbeat",
        "vram.state",
        "vram.leases",
        "vram.cache",
        "vram.evict",
    ] {
        assert!(
            registered.iter().any(|n| n == expected),
            "expected action {expected} not in registered set"
        );
    }
}
