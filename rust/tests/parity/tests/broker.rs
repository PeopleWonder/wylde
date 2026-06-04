//! VRAM broker pipe parity: Python `Core.resource_monitor.run` vs
//! `wylde-vram-broker.exe`.
//!
//! RETIRED: the Python broker (`Core.resource_monitor.run`) was deleted in
//! 7072947 — the vram-broker is rust-only now, so there is no Python half
//! left to diff against. The test below is `#[ignore]`d rather than removed
//! to keep the capture/diff scaffold should a future second implementation
//! ever need parity again.
//!
//! Both implementations bind the same canonical pipe with no name override,
//! so the harness captures sequentially: a fixed action script is replayed
//! against a fresh Python broker, then against a fresh Rust broker, and the
//! two reply lists are diffed. A fresh process per implementation means each
//! is exercised from identical empty state — the fair comparison for a
//! stateful service. See `wylde_parity::pipe` for the rationale.

#![cfg(feature = "parity")]

use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use wylde_parity::{diff, paths, pipe, proc};

/// `send_action` service name -> pipe `\\.\pipe\wylde-vram-broker`.
const SERVICE: &str = "vram-broker";

/// Fields that legitimately differ between two runs / two processes:
/// per-request timestamps, UUID lease ids, the broker pid, and the live
/// NVML usage reading. Stable hardware facts (`gpu.total_bytes`,
/// `gpu.name`) and config are deliberately NOT normalized — if those
/// diverge between implementations it is a real finding.
const VOLATILE: &[&str] = &[
    "error.message",
    "error.details",
    "data.lease_id",
    "data.granted_at",
    "data.expires_at",
    "data.heartbeat_at",
    "data.pid",
    "data.generated_at",
    "data.gpu.actual_used_bytes",
    "data.gpu.nvml_fresh_s",
    "data.leases.*.lease_id",
    "data.leases.*.granted_at",
    "data.leases.*.expires_at",
    "data.leases.*.heartbeat_at",
    "data.leases.*.pid",
    "data.entries.*.last_used",
    "data.entries.*.warm_for",
    "data.model_cache.entries.*.last_used",
    "data.model_cache.entries.*.warm_for",
];

/// One PiB — larger than any real GPU, so a reserve of this size is always
/// rejected with `would_exceed_total` regardless of the host hardware.
const ONE_PIB: u64 = 1 << 50;

/// Replay the fixed action script against one broker process and return the
/// `(case_name, reply_json)` list. The broker is killed before returning.
async fn capture(label: &str, cmd: Command) -> Vec<(&'static str, Value)> {
    let mut svc = proc::Service::spawn(label, cmd).expect("spawn broker");
    let ready = pipe::wait_ready(SERVICE, "vram.state", Duration::from_secs(25)).await;
    assert!(
        ready,
        "{label}: vram-broker pipe never became ready (process exited early: {})",
        svc.has_exited(),
    );

    let mut out: Vec<(&'static str, Value)> = Vec::new();

    // ── Error / read paths against a fresh, empty broker ──────────────
    out.push((
        "heartbeat_unknown",
        pipe::capture(SERVICE, "vram.heartbeat", json!({"lease_id": "parity-missing"})).await,
    ));
    out.push((
        "evict_unknown",
        pipe::capture(SERVICE, "vram.evict", json!({"lease_id": "parity-missing"})).await,
    ));
    out.push((
        "release_unknown",
        pipe::capture(SERVICE, "vram.release", json!({"lease_id": "parity-missing"})).await,
    ));
    out.push((
        "leases_empty",
        pipe::capture(SERVICE, "vram.leases", json!({})).await,
    ));
    out.push((
        "cache_empty",
        pipe::capture(SERVICE, "vram.cache", json!({})).await,
    ));
    out.push((
        "state_fresh",
        pipe::capture(SERVICE, "vram.state", json!({})).await,
    ));

    // ── Reserve too large -> rejected the same way on any hardware ─────
    out.push((
        "reserve_huge",
        pipe::capture(
            SERVICE,
            "vram.reserve",
            json!({"service": "parity", "model": "huge", "bytes": ONE_PIB, "priority": 50, "ttl": 60}),
        )
        .await,
    ));

    // ── Reserve a tiny lease, then exercise its lifecycle ─────────────
    let reserve_tiny = pipe::capture(
        SERVICE,
        "vram.reserve",
        json!({"service": "parity", "model": "tiny", "bytes": 1_048_576, "priority": 50, "ttl": 60}),
    )
    .await;
    // The lease id is this process's own — used for the release/heartbeat
    // steps below. If the reserve was rejected, fall back to a sentinel so
    // both implementations still run an identical script.
    let lease_id = reserve_tiny
        .get("data")
        .and_then(|d| d.get("lease_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "parity-missing-lease".to_string());
    out.push(("reserve_tiny", reserve_tiny));

    out.push((
        "state_after_reserve",
        pipe::capture(SERVICE, "vram.state", json!({})).await,
    ));
    out.push((
        "leases_after_reserve",
        pipe::capture(SERVICE, "vram.leases", json!({})).await,
    ));
    out.push((
        "release_lease",
        pipe::capture(SERVICE, "vram.release", json!({"lease_id": lease_id.clone()})).await,
    ));
    out.push((
        "heartbeat_after_release",
        pipe::capture(SERVICE, "vram.heartbeat", json!({"lease_id": lease_id, "ttl": 30})).await,
    ));

    drop(svc); // kill the broker before the next capture binds the pipe
    out
}

#[tokio::test]
#[ignore = "Python target Core.resource_monitor.run deleted in 7072947; \
            vram-broker is rust-only now — no Python half to diff against"]
async fn vram_broker_envelope_parity() {
    paths::require_artifact(
        &paths::venv_python(),
        "create the Wylde virtualenv (.venv) with the service dependencies",
    );
    paths::require_artifact(
        &paths::rust_release_bin("wylde-vram-broker"),
        "run `cargo build --release` in the rust/ workspace",
    );

    // Pre-flight: a production broker on the canonical pipe would make the
    // capture non-deterministic. Abort with a clear message instead.
    assert!(
        !pipe::pipe_in_use(SERVICE, "vram.state").await,
        "a vram-broker is already bound to \\\\.\\pipe\\wylde-vram-broker — \
         stop the running broker before running parity tests",
    );

    let python = capture(
        "python vram-broker",
        proc::python_module("Core.resource_monitor.run"),
    )
    .await;

    // Let the OS fully tear down the Python broker's pipe before the Rust
    // broker binds the same name.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let rust = capture("rust vram-broker", proc::rust_binary("wylde-vram-broker")).await;

    assert_eq!(
        python.len(),
        rust.len(),
        "capture scripts produced different case counts",
    );

    let mut failures: Vec<String> = Vec::new();
    for ((py_name, py_val), (rs_name, rs_val)) in python.iter().zip(&rust) {
        assert_eq!(py_name, rs_name, "case order mismatch");
        match diff::compare(py_name, py_val, rs_val, VOLATILE) {
            Ok(()) => eprintln!("[broker] {py_name} : PARITY"),
            Err(report) => failures.push(report),
        }
    }

    eprintln!("\n=== VRAM broker parity: {} cases ===", python.len());
    assert!(
        failures.is_empty(),
        "{} broker action(s) diverged:\n\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}
