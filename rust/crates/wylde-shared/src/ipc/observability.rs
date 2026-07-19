//! Per-call audit log — appends one JSON record per IPC round-trip.
//!
//! Rust port of `Core/shared/ipc/_observability.py`. The log path defaults
//! to `logs/ipc.jsonl` and is overridden by `WYLDE_IPC_LOG`. Records mirror
//! the Python shape one-for-one so a single tail can mix Python and Rust
//! service traffic.

use std::path::Path;

use crate::ipc::wire::{EnvConfig, Reply};
use crate::logging::rotating_sink;

/// Append one audit-log line for a completed call.
///
/// The shape (field names, order, types) matches Python's `_log_call`
/// exactly. Failures are swallowed — a broken audit log must never crash
/// the call site.
pub fn log_call(service: &str, method: &str, reply: &Reply, bytes_in: usize, bytes_out: usize) {
    let cfg = EnvConfig::load();
    log_call_to(
        &cfg.log_path,
        &cfg.self_name,
        service,
        method,
        reply,
        bytes_in,
        bytes_out,
    );
}

/// Same as [`log_call`] but with an explicit target path / caller name —
/// for tests that need to redirect the log without touching the env.
pub fn log_call_to(
    path: &Path,
    caller: &str,
    service: &str,
    method: &str,
    reply: &Reply,
    bytes_in: usize,
    bytes_out: usize,
) {
    let _ = write_record(path, caller, service, method, reply, bytes_in, bytes_out);
}

fn write_record(
    path: &Path,
    caller: &str,
    service: &str,
    method: &str,
    reply: &Reply,
    bytes_in: usize,
    bytes_out: usize,
) -> std::io::Result<()> {
    let mut record = serde_json::Map::new();
    record.insert(
        "ts".into(),
        serde_json::Value::String(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    record.insert("caller".into(), serde_json::Value::String(caller.into()));
    record.insert("callee".into(), serde_json::Value::String(service.into()));
    record.insert("method".into(), serde_json::Value::String(method.into()));
    record.insert(
        "transport".into(),
        serde_json::Value::String(reply.transport.clone()),
    );
    record.insert(
        "bytes_in".into(),
        serde_json::Value::Number(serde_json::Number::from(bytes_in)),
    );
    record.insert(
        "bytes_out".into(),
        serde_json::Value::Number(serde_json::Number::from(bytes_out)),
    );
    // Round to 3 decimals to match Python's `round(reply.duration_ms, 3)`.
    let dur = (reply.duration_ms * 1000.0).round() / 1000.0;
    record.insert(
        "dur_ms".into(),
        serde_json::Number::from_f64(dur)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
    );
    record.insert("ok".into(), serde_json::Value::Bool(reply.ok));
    if !reply.ok {
        if let Some(err) = &reply.error {
            record.insert(
                "err_code".into(),
                serde_json::Value::String(err.code.clone()),
            );
        }
    }

    let line = serde_json::to_string(&record)?;
    // Route through the shared rotating sink so `ipc.jsonl` inherits the
    // size + retention policy by construction — no per-file cap here.
    rotating_sink(path).write_line(&line)
}

/// Cheap size hint for a payload — matches Python's `_size`.
pub fn payload_size(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null => 0,
        serde_json::Value::String(s) => s.len(),
        other => serde_json::to_string(other).map(|s| s.len()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_audit_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ipc.jsonl");
        let mut reply = Reply::ok(serde_json::json!({"x": 1}));
        reply.transport = "pipe".into();
        reply.duration_ms = 12.5;
        log_call_to(&path, "test-caller", "test-svc", "/echo", &reply, 11, 22);
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("\"caller\":\"test-caller\""));
        assert!(body.contains("\"callee\":\"test-svc\""));
        assert!(body.contains("\"method\":\"/echo\""));
        assert!(body.contains("\"transport\":\"pipe\""));
        assert!(body.contains("\"ok\":true"));
        assert!(body.contains("\"bytes_in\":11"));
        assert!(body.contains("\"bytes_out\":22"));
    }

    #[test]
    fn writes_err_code_on_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ipc.jsonl");
        let reply = Reply::err_msg("pipe_unavailable", "no pipe");
        log_call_to(&path, "caller", "svc", "/x", &reply, 0, 0);
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("\"err_code\":\"pipe_unavailable\""));
        assert!(body.contains("\"ok\":false"));
    }

    #[test]
    fn payload_size_matches_python_shape() {
        // null → 0
        assert_eq!(payload_size(&serde_json::Value::Null), 0);
        // string → utf-8 byte length
        assert_eq!(payload_size(&serde_json::Value::String("abc".into())), 3);
        // map → json-serialized length
        let m = serde_json::json!({"a": 1});
        assert_eq!(payload_size(&m), serde_json::to_string(&m).unwrap().len());
    }
}
