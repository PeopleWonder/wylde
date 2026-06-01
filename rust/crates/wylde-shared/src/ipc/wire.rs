//! Wire-level config + datatypes for the shared IPC transport.
//!
//! Rust port of `Core/shared/ipc/_wire.py`. The on-wire frame format is
//! `[u32 BE length][rmp-serde body]` — bytes are produced/consumed in the
//! exact shape the live Python services emit, so the two halves of the
//! system stay binary-compatible through the migration.
//!
//! Public types ([`Reply`], [`IpcError`]) and frame helpers ([`encode_frame`],
//! [`decode_frame`]) live here; the client / server / actions submodules
//! pull them through [`crate::ipc`].

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Wire protocol version. Mirrors Python's `_wire.IPC_VERSION`.
///
/// Bumped when the envelope shape or handshake changes in a breaking way.
/// Encoded on the wire as an msgpack integer; v1 is the first version to
/// carry a handshake frame.
pub const IPC_VERSION: u32 = 1;

/// Heartbeat cadence for streaming replies. The server emits a null-payload
/// chunk every `STREAM_HEARTBEAT_SECS` of handler silence so the client's
/// `WYLDE_IPC_IDLE_TIMEOUT` (default 300s) doesn't fire mid-stream. Picked
/// well under the 30s headroom alluded to in the streaming spec — 25s gives
/// the chunk time to land before any 30s downstream timer trips.
pub const STREAM_HEARTBEAT_SECS: u64 = 25;

/// Default per-call timeout. Overridden by `WYLDE_IPC_TIMEOUT`.
pub const DEFAULT_TIMEOUT_SECS: f64 = 30.0;

/// Handshake-phase timeout in seconds.
pub const HANDSHAKE_TIMEOUT_SECS: f64 = 5.0;

/// Pipe connect retry deadline in milliseconds (matches Python).
pub const PIPE_CONNECT_TIMEOUT_MS: u64 = 2000;

/// Max frame size — anything larger is treated as a corrupted stream.
/// Matches Python's 64 MiB cap (`_PipeHandle.read_frame`).
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Structured IPC error.
///
/// Matches the Python wire shape `{"code": <str>, "message": <str>, "details"?: <map>}`.
/// The `code` field is the stable wire identifier (e.g. `pipe_unavailable`,
/// `handshake_timeout`); the dynamic `message` is human-readable.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct IpcError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl IpcError {
    /// Build a fresh error with just code/message — no details.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }
}

/// In-process reply value returned by [`crate::ipc::send`] / [`crate::ipc::call`].
///
/// Wire fields (`ok`, `data`, `error`) match the Python `Reply` dataclass
/// and the on-wire envelope produced by the Python pipe server. The
/// `transport` / `duration_ms` fields are filled in locally by the client
/// for the audit log; they never travel over the wire.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Reply {
    pub ok: bool,

    #[serde(default, skip_serializing_if = "is_null_value")]
    pub data: serde_json::Value,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,

    /// Local-only annotation: `"pipe"`, `"http"`, `"none"`, or empty.
    /// Not part of the wire envelope.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub transport: String,

    /// Local-only annotation: round-trip duration in milliseconds.
    /// Not part of the wire envelope.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub duration_ms: f64,
}

fn is_null_value(v: &serde_json::Value) -> bool {
    v.is_null()
}

/// One frame of a streaming reply.
///
/// Streaming replies use multiple frames on the same connection, all sharing
/// the request-correlation `id` (a UUID hex string — matching the existing
/// unary reply shape; the Phase 0 spec called it `u64` but we kept String to
/// stay consistent with the live `ReplyFrame`). `seq` starts at 0 and
/// increments per frame. `done=true` terminates the stream; the accompanying
/// `payload` may be `null` or carry the final chunk.
///
/// A null-payload, `done=false` frame is a heartbeat — the client must read
/// it and silently discard it, treating it only as evidence that the server
/// is still alive.
///
/// `ok=false` with `error: Some(_)` carries a stream-level error. The client
/// surfaces it as an `Err` item; the server always sets `done=true` on an
/// error frame so the stream terminates immediately after.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkFrame {
    pub id: String,
    pub seq: u32,
    #[serde(default, skip_serializing_if = "is_null_value")]
    pub payload: serde_json::Value,
    pub done: bool,
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

fn default_true() -> bool {
    true
}

impl Reply {
    /// Construct an `ok=true` reply with the given data payload.
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data,
            error: None,
            transport: String::new(),
            duration_ms: 0.0,
        }
    }

    /// Construct an `ok=false` reply with the given error.
    pub fn err(error: IpcError) -> Self {
        Self {
            ok: false,
            data: serde_json::Value::Null,
            error: Some(error),
            transport: String::new(),
            duration_ms: 0.0,
        }
    }

    /// Convenience: build an `ok=false` reply from code + message.
    pub fn err_msg(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::err(IpcError::new(code, message))
    }
}

/// Env-derived configuration. Read once at first access — env mutations after
/// process start do not retroactively change behaviour, matching the Python
/// module-import semantics.
#[derive(Debug, Clone)]
pub struct EnvConfig {
    pub transport: String,
    pub ipc_disable: bool,
    pub default_timeout: f64,
    pub log_path: PathBuf,
    pub self_name: String,
    pub frame_read_timeout: f64,
    pub idle_read_timeout: f64,
    pub handshake_timeout: f64,
}

impl EnvConfig {
    /// Snapshot the IPC-relevant env vars.
    pub fn load() -> Self {
        let transport = std::env::var("WYLDE_TRANSPORT")
            .unwrap_or_else(|_| "pipe".to_string())
            .trim()
            .to_lowercase();
        let transport = if transport == "pipe" || transport == "http" {
            transport
        } else {
            "pipe".to_string()
        };
        let ipc_disable = matches!(
            std::env::var("WYLDE_IPC_DISABLE")
                .unwrap_or_default()
                .to_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        );
        let default_timeout = std::env::var("WYLDE_IPC_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let log_path = PathBuf::from(
            std::env::var("WYLDE_IPC_LOG").unwrap_or_else(|_| "logs/ipc.jsonl".into()),
        );
        let self_name = std::env::var("WYLDE_SERVICE_NAME").unwrap_or_else(|_| "unknown".into());
        let frame_read_timeout = std::env::var("WYLDE_IPC_READ_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30.0);
        let idle_read_timeout = std::env::var("WYLDE_IPC_IDLE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300.0);
        let handshake_timeout = std::env::var("WYLDE_IPC_HANDSHAKE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(HANDSHAKE_TIMEOUT_SECS);

        Self {
            transport,
            ipc_disable,
            default_timeout,
            log_path,
            self_name,
            frame_read_timeout,
            idle_read_timeout,
            handshake_timeout,
        }
    }
}

/// Encode a body as `[u32 BE length][payload]`. Mirrors Python's
/// `header = len(payload).to_bytes(4, "big"); WriteFile(handle, header + payload)`.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut out = Vec::with_capacity(4 + n);
    out.extend_from_slice(&(n as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode a single length-prefixed frame from a synchronous reader.
///
/// Returns the body bytes (without the 4-byte length header). Errors mirror
/// Python's `_PipeHandle.read_frame`: zero-length and over-cap frames are
/// rejected as malformed streams.
pub fn decode_frame<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let n = u32::from_be_bytes(header) as usize;
    if n == 0 || n > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pipe frame size out of range: {n}"),
        ));
    }
    let mut body = vec![0u8; n];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// Async sibling of [`decode_frame`] for tokio readers.
pub async fn decode_frame_async<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;
    let n = u32::from_be_bytes(header) as usize;
    if n == 0 || n > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pipe frame size out of range: {n}"),
        ));
    }
    let mut body = vec![0u8; n];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

/// Write a length-prefixed frame to a synchronous writer.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    let header = (payload.len() as u32).to_be_bytes();
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    Ok(())
}

/// Async sibling of [`write_frame`].
pub async fn write_frame_async<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let header = (payload.len() as u32).to_be_bytes();
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Pipe name for a given service: `\\.\pipe\wylde-<service>` with the
/// `wylde-` prefix stripped if the caller already supplied it.
/// Matches Python's `_pipe_name`.
pub fn pipe_name(service: &str) -> String {
    let bare = service.strip_prefix("wylde-").unwrap_or(service);
    format!(r"\\.\pipe\wylde-{bare}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let body = b"hello world";
        let framed = encode_frame(body);
        assert_eq!(&framed[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&framed[4..], body);

        let mut cur = Cursor::new(framed);
        let out = decode_frame(&mut cur).expect("decode");
        assert_eq!(out, body);
    }

    #[test]
    fn frame_rejects_zero_length() {
        let mut cur = Cursor::new(vec![0u8, 0, 0, 0]);
        let err = decode_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_rejects_oversize() {
        let mut framed = (MAX_FRAME_SIZE as u32 + 1).to_be_bytes().to_vec();
        framed.push(0); // one byte body so the read doesn't EOF first
        let mut cur = Cursor::new(framed);
        let err = decode_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_rejects_truncated_header() {
        let mut cur = Cursor::new(vec![0u8, 0]);
        let err = decode_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frame_rejects_truncated_body() {
        // header says 10 bytes, only 3 follow
        let mut framed = (10u32).to_be_bytes().to_vec();
        framed.extend_from_slice(b"abc");
        let mut cur = Cursor::new(framed);
        let err = decode_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn pipe_name_strips_wylde_prefix() {
        assert_eq!(pipe_name("vram-broker"), r"\\.\pipe\wylde-vram-broker");
        assert_eq!(pipe_name("wylde-vram-broker"), r"\\.\pipe\wylde-vram-broker");
    }

    #[test]
    fn reply_serializes_without_optional_fields() {
        let r = Reply::ok(serde_json::json!({"pong": true}));
        // rmp-serde encode -> decode -> json view
        let bytes = rmp_serde::to_vec_named(&r).expect("encode");
        let v: serde_json::Value = rmp_serde::from_slice(&bytes).expect("decode");
        // ok and data must be present; error must be absent (skip_serializing_if).
        assert_eq!(v["ok"], serde_json::Value::Bool(true));
        assert_eq!(v["data"]["pong"], serde_json::Value::Bool(true));
        assert!(v.get("error").is_none() || v["error"].is_null());
    }

    #[test]
    fn reply_err_has_error_no_data() {
        let r = Reply::err_msg("not_found", "missing");
        let bytes = rmp_serde::to_vec_named(&r).expect("encode");
        let v: serde_json::Value = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(v["ok"], serde_json::Value::Bool(false));
        assert_eq!(v["error"]["code"], "not_found");
        assert_eq!(v["error"]["message"], "missing");
    }

    #[test]
    fn ipc_error_display_format() {
        let e = IpcError::new("pipe_connect", "could not connect");
        assert_eq!(format!("{e}"), "pipe_connect: could not connect");
    }

    #[test]
    fn env_config_defaults() {
        let cfg = EnvConfig::load();
        // Don't assert specific values (env may be set by the harness), just
        // that the snapshot is well-formed.
        assert!(cfg.transport == "pipe" || cfg.transport == "http");
        assert!(cfg.default_timeout > 0.0);
    }
}
