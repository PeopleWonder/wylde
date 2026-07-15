//! A deliberately tiny, synchronous client for the Wylde IPC named pipes.
//!
//! The lifecycle daemon and every service speak the wire protocol defined in
//! `wylde_shared::ipc::wire`: each frame is `[u32 big-endian length][rmp-serde
//! body]`, and a connection is a v1 handshake frame (`{wylde_ipc, caller,
//! service}` → server ack) followed by request/reply frames
//! (`{id, method, http_verb, data}` → `{id, ok, data, error}`). Action verbs
//! ride `method = "/__action__"` with `data = {action, payload}`; `/__ping__`
//! is the native liveness method.
//!
//! We do **not** depend on `wylde-shared` (async, tokio named pipes, futures,
//! uuid) for the launch-verify gate — that would drag the whole backend IPC
//! stack into this standalone dev tool. Instead we hand-roll the handshake +
//! one round-trip over blocking `std::fs::File` pipe I/O, encoding the frames
//! with `rmp-serde` so they stay byte-identical to what the server expects.
//!
//! **Fail-closed timeouts.** Blocking pipe I/O has no per-read deadline, so
//! every call runs on a worker thread joined with `recv_timeout`; if the server
//! hangs, the call returns an error (and the gate fails) rather than wedging the
//! whole preflight. The abandoned worker dies with the process.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// Windows `ERROR_PIPE_BUSY` — all pipe instances are momentarily in use; the
/// server is up, just retry the open.
const ERROR_PIPE_BUSY: i32 = 231;

/// `\\.\pipe\wylde-<bare>` for a service, matching `wylde_shared::ipc::wire::
/// pipe_name` (strips a leading `wylde-` if the caller already supplied it).
pub fn pipe_path(service: &str) -> String {
    let bare = service.strip_prefix("wylde-").unwrap_or(service);
    format!(r"\\.\pipe\wylde-{bare}")
}

/// Is a server currently bound to this service's pipe? A successful open — or
/// `ERROR_PIPE_BUSY` (bound but all instances busy) — means yes; a
/// file-not-found means no server. Any other error is treated as "not
/// reachable" (fail-closed).
pub fn pipe_exists(service: &str) -> bool {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path(service))
    {
        Ok(_) => true,
        Err(e) => e.raw_os_error() == Some(ERROR_PIPE_BUSY),
    }
}

/// Monotone-ish request id. The server only echoes it for correlation on a
/// single-request connection, so uniqueness (not RFC-4122 shape) is all that
/// matters — a counter mixed with the process-start nanos avoids a `uuid` dep.
fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wr-{nanos:x}-{n:x}")
}

/// Open the pipe, retrying only while it reports `ERROR_PIPE_BUSY` (server up,
/// instances momentarily exhausted). A missing pipe fails immediately — the
/// caller decides whether that's expected.
fn connect(path: &str, connect_timeout: Duration) -> Result<std::fs::File> {
    let deadline = Instant::now() + connect_timeout;
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e).with_context(|| format!("opening pipe {path}")),
        }
    }
}

fn write_frame(pipe: &mut std::fs::File, body: &[u8]) -> Result<()> {
    let header = (body.len() as u32).to_be_bytes();
    pipe.write_all(&header).context("writing frame header")?;
    pipe.write_all(body).context("writing frame body")?;
    pipe.flush().context("flushing frame")?;
    Ok(())
}

fn read_frame(pipe: &mut std::fs::File) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    pipe.read_exact(&mut header).context("reading frame header")?;
    let n = u32::from_be_bytes(header) as usize;
    // Mirror the server's malformed-stream guards (0 or > 64 MiB).
    if n == 0 || n > 64 * 1024 * 1024 {
        bail!("pipe frame size out of range: {n}");
    }
    let mut body = vec![0u8; n];
    pipe.read_exact(&mut body).context("reading frame body")?;
    Ok(body)
}

/// One handshake + one request/reply on a fresh connection, returning the
/// decoded reply envelope as a `serde_json::Value` (`{ok, data, error}`).
fn roundtrip_blocking(service: &str, method: &str, data: Value) -> Result<Value> {
    let mut pipe = connect(&pipe_path(service), Duration::from_secs(2))?;

    // v1 handshake — the server acks with `{wylde_ipc, ok, service}`; we only
    // need to consume the ack frame before sending the request.
    let handshake = rmp_serde::to_vec_named(&json!({
        "wylde_ipc": 1,
        "caller": "wylde-release",
        "service": service,
    }))
    .context("encoding handshake")?;
    write_frame(&mut pipe, &handshake)?;
    let _ack = read_frame(&mut pipe).context("reading handshake ack")?;

    let request = rmp_serde::to_vec_named(&json!({
        "id": next_id(),
        "method": method,
        "http_verb": "POST",
        "data": data,
    }))
    .context("encoding request")?;
    write_frame(&mut pipe, &request)?;

    let reply_body = read_frame(&mut pipe).context("reading reply")?;
    let reply: Value =
        rmp_serde::from_slice(&reply_body).context("decoding msgpack reply envelope")?;
    Ok(reply)
}

/// Run a pipe round-trip on a worker thread, bounded by `timeout`. A hung
/// server surfaces as an error (fail-closed) instead of blocking forever.
fn with_timeout(
    service: &str,
    method: &str,
    data: Value,
    timeout: Duration,
) -> Result<Value> {
    let (tx, rx) = mpsc::channel();
    let (service, method) = (service.to_string(), method.to_string());
    thread::spawn(move || {
        let _ = tx.send(roundtrip_blocking(&service, &method, data));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(anyhow!("pipe call timed out after {}s", timeout.as_secs_f64())),
    }
}

/// Native liveness ping. Returns the `data` payload (`{pong, ver}`) on success.
pub fn ping(service: &str, timeout: Duration) -> Result<Value> {
    let reply = with_timeout(service, "/__ping__", json!({}), timeout)?;
    check_ok(reply)
}

/// Invoke an action verb (`service.list`, `vram.state`, …) and return its
/// `data` payload on an `ok` reply, or an error carrying the server's error
/// code/message on a failure reply.
pub fn action(service: &str, verb: &str, payload: Value, timeout: Duration) -> Result<Value> {
    let data = json!({ "action": verb, "payload": payload });
    let reply = with_timeout(service, "/__action__", data, timeout)?;
    check_ok(reply)
}

/// Unwrap an `{ok, data, error}` reply envelope into its `data`, turning an
/// `ok=false` reply into an `Err` with the server's `code: message`.
fn check_ok(reply: Value) -> Result<Value> {
    let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if ok {
        Ok(reply.get("data").cloned().unwrap_or(Value::Null))
    } else {
        let err = reply.get("error");
        let code = err
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let msg = err
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        bail!("service replied not-ok: {code}: {msg}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_path_matches_wire_convention() {
        assert_eq!(pipe_path("wylde-lifecycle"), r"\\.\pipe\wylde-lifecycle");
        assert_eq!(pipe_path("lifecycle"), r"\\.\pipe\wylde-lifecycle");
        assert_eq!(pipe_path("vram-broker"), r"\\.\pipe\wylde-vram-broker");
    }

    #[test]
    fn next_id_is_unique() {
        let a = next_id();
        let b = next_id();
        assert_ne!(a, b);
        assert!(a.starts_with("wr-"));
    }

    #[test]
    fn check_ok_extracts_data() {
        let reply = json!({"ok": true, "data": {"pong": true, "ver": 1}});
        let data = check_ok(reply).unwrap();
        assert_eq!(data["pong"], json!(true));
    }

    #[test]
    fn check_ok_surfaces_error_code_and_message() {
        let reply = json!({"ok": false, "error": {"code": "no_handler", "message": "nope"}});
        let err = check_ok(reply).unwrap_err().to_string();
        assert!(err.contains("no_handler"), "got: {err}");
        assert!(err.contains("nope"), "got: {err}");
    }

    #[test]
    fn check_ok_treats_missing_ok_as_failure() {
        // Fail-closed: an envelope with no `ok` field is not a pass.
        assert!(check_ok(json!({"data": {}})).is_err());
    }

    #[test]
    fn missing_pipe_reports_not_exists() {
        // A service nobody is serving must read as absent, not present.
        assert!(!pipe_exists("wylde-release-nonexistent-test-pipe"));
    }
}
