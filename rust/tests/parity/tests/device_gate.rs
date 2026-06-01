//! Device gate pipe parity: Python `device_gate.run` vs
//! `wylde-device-gate.exe`.
//!
//! Same sequential-capture strategy as the VRAM broker (see `broker.rs`).
//!
//! Unlike the broker, the device gate persists state to disk (the device
//! store), so the script is restricted to **read and error paths only** —
//! `list_devices`, `get_pairing_status`, bad-token `verify`, and operations
//! on a non-existent device. None of these mutate the store, so the Python
//! capture leaves identical state for the Rust capture, and the two are
//! diffed fairly. (Pairing-creation actions are intentionally omitted —
//! they would write to the store.)

#![cfg(feature = "parity")]

use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use wylde_parity::{diff, paths, pipe, proc};

/// `send_action` service name -> pipe `\\.\pipe\wylde-device-gate`.
const SERVICE: &str = "wylde-device-gate";

/// Error wording and per-response computed times differ legitimately;
/// stored device fields do not (both implementations read the same store).
const VOLATILE: &[&str] = &[
    "error.message",
    "error.details",
    "data.ts",
    "data.expires_in",
    "data.seconds_remaining",
];

/// A device id that does not exist — every mutating action against it is
/// rejected without touching the store.
const ABSENT_DEVICE: &str = "parity-nonexistent-device";

async fn capture(label: &str, cmd: Command) -> Vec<(&'static str, Value)> {
    let mut svc = proc::Service::spawn(label, cmd).expect("spawn device gate");
    let ready = pipe::wait_ready(SERVICE, "device_gate.get_pairing_status", Duration::from_secs(25)).await;
    assert!(
        ready,
        "{label}: device-gate pipe never became ready (process exited early: {})",
        svc.has_exited(),
    );

    let mut out: Vec<(&'static str, Value)> = Vec::new();

    out.push((
        "pairing_status",
        pipe::capture(SERVICE, "device_gate.get_pairing_status", json!({})).await,
    ));
    out.push((
        "list_devices",
        pipe::capture(SERVICE, "device_gate.list_devices", json!({})).await,
    ));
    out.push((
        "verify_bad_token",
        pipe::capture(SERVICE, "device_gate.verify", json!({"token": "parity-bogus-token"})).await,
    ));
    out.push((
        "consume_events_absent",
        pipe::capture(
            SERVICE,
            "device_gate.consume_pending_events",
            json!({"device_id": ABSENT_DEVICE}),
        )
        .await,
    ));
    out.push((
        "set_tier_absent",
        pipe::capture(
            SERVICE,
            "device_gate.set_tier",
            json!({"device_id": ABSENT_DEVICE, "tier": "trusted"}),
        )
        .await,
    ));
    out.push((
        "rotate_token_absent",
        pipe::capture(
            SERVICE,
            "device_gate.rotate_token",
            json!({"device_id": ABSENT_DEVICE}),
        )
        .await,
    ));
    out.push((
        "revoke_absent",
        pipe::capture(SERVICE, "device_gate.revoke", json!({"device_id": ABSENT_DEVICE})).await,
    ));
    out.push((
        "complete_pairing_bad_code",
        pipe::capture(
            SERVICE,
            "device_gate.complete_pairing",
            json!({"code": "000000", "username": "parity-user", "password": "parity-pw"}),
        )
        .await,
    ));

    drop(svc);
    out
}

#[tokio::test]
async fn device_gate_envelope_parity() {
    paths::require_artifact(
        &paths::venv_python(),
        "create the Wylde virtualenv (.venv) with the service dependencies",
    );
    paths::require_artifact(
        &paths::rust_release_bin("wylde-device-gate"),
        "run `cargo build --release` in the rust/ workspace",
    );

    assert!(
        !pipe::pipe_in_use(SERVICE, "device_gate.get_pairing_status").await,
        "a device-gate is already bound to \\\\.\\pipe\\wylde-device-gate — \
         stop the running service before running parity tests",
    );

    let python = capture("python device-gate", proc::python_module("device_gate.run")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rust = capture("rust device-gate", proc::rust_binary("wylde-device-gate")).await;

    assert_eq!(python.len(), rust.len(), "capture scripts produced different case counts");

    let mut failures: Vec<String> = Vec::new();
    for ((py_name, py_val), (rs_name, rs_val)) in python.iter().zip(&rust) {
        assert_eq!(py_name, rs_name, "case order mismatch");
        match diff::compare(py_name, py_val, rs_val, VOLATILE) {
            Ok(()) => eprintln!("[device-gate] {py_name} : PARITY"),
            Err(report) => failures.push(report),
        }
    }

    // Root-cause hint: if the Python side collapsed structured errors to the
    // generic `handler` code, the divergence is a known Python bug, not a
    // Rust defect — see the report. Detecting it keeps the gate output
    // self-explanatory.
    let python_collapsed = python.iter().any(|(_, value)| {
        value.pointer("/error/code").and_then(serde_json::Value::as_str) == Some("handler")
    });
    let diagnosis = if python_collapsed {
        "\n\nROOT CAUSE: the Python device_gate `_wrap_handler` (device_gate/pipe.py) \
         re-raises structured `_ActionError`s as a plain `RuntimeError`, so every error \
         reply collapses to `error.code = \"handler\"` (the real code is buried in the \
         message as `[code] ...`). The Rust port returns the correct structured codes. \
         This is a Python bug; device-gate cutover is blocked until the Python wrapper \
         preserves the code. The Rust source was left unchanged."
    } else {
        ""
    };

    eprintln!("\n=== Device gate parity: {} cases ===", python.len());
    assert!(
        failures.is_empty(),
        "{} device-gate action(s) diverged:\n\n{}{}",
        failures.len(),
        failures.join("\n\n"),
        diagnosis,
    );
}
