//! Named-pipe capture for VRAM broker / device gate parity.
//!
//! Both implementations of a pipe service bind the *same* canonical pipe
//! (`\\.\pipe\wylde-<service>`) — the name is derived from a hardcoded
//! service constant with no env override. Running the Python and Rust
//! servers literally side-by-side would therefore make a client connect to
//! a non-deterministic instance.
//!
//! So the harness captures **sequentially**: spin up one implementation,
//! replay a fixed action script against it, collect every reply, kill it;
//! then do the same for the other implementation; then diff the two reply
//! lists. For a stateful service this is also the *fairer* test — each
//! implementation is exercised from an identical fresh state.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wylde_shared::ipc::{send, send_action, Reply};

/// Render a [`Reply`] as a plain JSON value for diffing. The wire envelope
/// is exactly `{ ok, data, error }`; `transport` / `duration_ms` are
/// client-local annotations and deliberately excluded.
pub fn reply_to_json(reply: &Reply) -> Value {
    json!({
        "ok": reply.ok,
        "data": reply.data,
        "error": reply.error.as_ref().map(|e| json!({
            "code": e.code,
            "message": e.message,
            "details": e.details,
        })),
    })
}

/// Connect-level error codes that mean "the pipe server is not up yet" — as
/// opposed to a handler-level error, which means it *is* up and answering.
const NOT_READY_CODES: &[&str] = &[
    "pipe_connect",
    "pipe_unavailable",
    "pipe_timeout",
    "handshake_timeout",
    "handshake_io",
];

/// Send one action to `service` and capture the reply as JSON.
pub async fn capture(service: &str, action: &str, payload: Value) -> Value {
    reply_to_json(&send_action(service, action, payload).await)
}

/// Send one *raw method* request to `service` and capture the reply as JSON.
///
/// Unlike [`capture`] — which wraps the request in the `/__action__`
/// dispatch envelope — this fires `method` verbatim. Used for the pipe
/// server's built-in control frames (`/__ping__`, `/__handshake__`),
/// which both the Python and Rust pipe servers answer in-band.
pub async fn capture_method(service: &str, method: &str, data: Value) -> Value {
    reply_to_json(&send(service, method, data, Duration::from_secs(10)).await)
}

/// Poll `service` with `probe_action` until the pipe answers (with anything
/// other than a connect-level error), or `timeout` elapses.
pub async fn wait_ready(service: &str, probe_action: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let reply = send_action(service, probe_action, json!({})).await;
        if reply.ok {
            return true;
        }
        let connecting = reply
            .error
            .as_ref()
            .map(|e| NOT_READY_CODES.contains(&e.code.as_str()))
            .unwrap_or(false);
        if !connecting {
            // Server responded with a handler-level reply: it is up.
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Is a pipe service already bound to this name? Used as a pre-flight guard:
/// a production instance occupying the canonical pipe would make captures
/// non-deterministic, so the test aborts with a clear message instead.
pub async fn pipe_in_use(service: &str, probe_action: &str) -> bool {
    let reply = send_action(service, probe_action, json!({})).await;
    if reply.ok {
        return true;
    }
    reply
        .error
        .as_ref()
        .map(|e| !NOT_READY_CODES.contains(&e.code.as_str()))
        .unwrap_or(false)
}
