//! Outbound IPC: `send` / `call` / action helpers and the pipe client.
//!
//! Rust port of `Core/shared/ipc/_client.py`. The caller side of the pipe
//! protocol — connects to `\\.\pipe\wylde-<service>`, performs the v1
//! handshake (matching the Python wire shape), then exchanges one or more
//! request/reply frames.
//!
//! Connection failure never panics: any error path returns a clean
//! `Reply { ok: false, error: Some(_) }` whose `error.code` mirrors the
//! Python identifiers (`pipe_unavailable`, `pipe_connect`, `pipe_timeout`,
//! `pipe_io`, `encode`, `decode`, `bad_response`).
//!
//! HTTP fallback from the Python implementation is intentionally NOT
//! ported here yet — the Rust ipc primitive is pipe-only (see W4.2 spec).
//! Callers that need HTTP fall through to the Python service stack.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::Stream;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ipc::observability::{log_call, payload_size};
use crate::ipc::wire::{pipe_name, ChunkFrame, EnvConfig, IpcError, Reply, IPC_VERSION};

// ── External request envelope shapes ──────────────────────────────────

/// Wire-level request envelope. Matches the Python producer.
#[derive(Serialize, Debug)]
struct RequestEnvelope<'a> {
    ver: u32,
    id: &'a str,
    method: &'a str,
    http_verb: &'a str,
    data: &'a serde_json::Value,
    meta: RequestMeta<'a>,
}

#[derive(Serialize, Debug)]
struct RequestMeta<'a> {
    deadline_ms: u64,
    caller: &'a str,
}

/// Wire-level reply envelope. Matches the Python producer.
#[derive(Deserialize, Debug)]
struct WireReply {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    data: serde_json::Value,
    #[serde(default)]
    error: Option<IpcError>,
}

#[derive(Serialize, Debug)]
struct HandshakeFrame<'a> {
    wylde_ipc: u32,
    caller: &'a str,
    service: &'a str,
}

#[derive(Deserialize, Debug)]
struct HandshakeResponse {
    #[serde(default)]
    wylde_ipc: Option<u32>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<IpcError>,
}

// ── Handler registry (currently a thin port; future work: route table) ──

static HANDLER_REGISTRY: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn handler_registry() -> &'static Mutex<HashMap<String, String>> {
    HANDLER_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that `method` maps to a route `path`. Mirrors Python's
/// `register_handler` — the pipe server consults this table when
/// dispatching. Idempotent.
pub fn register_handler(method: &str, path: Option<&str>) {
    let resolved = path
        .map(String::from)
        .unwrap_or_else(|| format!("/{method}"));
    let mut reg = handler_registry()
        .lock()
        .expect("handler registry poisoned");
    reg.insert(method.to_string(), resolved);
}

/// Look up a method's registered path. Falls back to `"/<method>"` when
/// unregistered, matching the Python behaviour.
#[allow(dead_code)] // used by the eventual route-table dispatcher; tests cover it now
pub(crate) fn resolve_handler_path(method: &str) -> String {
    let reg = handler_registry()
        .lock()
        .expect("handler registry poisoned");
    reg.get(method).cloned().unwrap_or_else(|| {
        if method.starts_with('/') {
            method.to_string()
        } else {
            format!("/{method}")
        }
    })
}

// ── Public API ────────────────────────────────────────────────────────

/// Fire one request at `service` and return a [`Reply`].
///
/// `data` is the request body (typically a JSON map). `timeout` bounds the
/// pipe handshake + write + read. On any pipe error, the returned `Reply`
/// is `ok=false` with a structured error code mirroring Python.
pub async fn send(
    service: &str,
    method: &str,
    data: serde_json::Value,
    timeout: Duration,
) -> Reply {
    send_with_verb(service, method, "POST", data, timeout).await
}

/// Like [`send`] but lets the caller pick the HTTP verb stamped on the
/// request envelope.
///
/// Python's `ipc.send` takes `http_verb`; some service `/health` endpoints
/// are GET-only, so callers porting Python that explicitly chose GET need
/// the same control here. Pipe transport is identical to [`send`]; only
/// the `http_verb` field in the request envelope differs.
pub async fn send_with_verb(
    service: &str,
    method: &str,
    http_verb: &str,
    data: serde_json::Value,
    timeout: Duration,
) -> Reply {
    let t0 = Instant::now();
    let cfg = EnvConfig::load();
    let bytes_in = payload_size(&data);

    let mut reply = if cfg.ipc_disable {
        Reply::err(IpcError::new(
            "ipc_disabled",
            "WYLDE_IPC_DISABLE is set; refusing to dispatch",
        ))
    } else if cfg.transport == "http" {
        // HTTP fallback is owned by the Python stack for now. Surface a
        // clean error rather than silently using a dead path.
        let mut r = Reply::err(IpcError::new(
            "no_http_backend",
            "Rust ipc has no HTTP fallback; set WYLDE_TRANSPORT=pipe",
        ));
        r.transport = "none".into();
        r
    } else {
        send_pipe(service, method, http_verb, &data, timeout, &cfg).await
    };

    if reply.transport.is_empty() {
        reply.transport = "pipe".into();
    }
    reply.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let bytes_out = payload_size(&reply.data);
    log_call(service, method, &reply, bytes_in, bytes_out);
    reply
}

/// Like [`send`] but uses [`EnvConfig::default_timeout`] for the deadline.
pub async fn call(service: &str, method: &str, data: serde_json::Value) -> Reply {
    let cfg = EnvConfig::load();
    send(
        service,
        method,
        data,
        Duration::from_secs_f64(cfg.default_timeout),
    )
    .await
}

/// Invoke a pipe-only action handler on `service`.
///
/// Wraps [`send`] with the `/__action__` envelope contract so callers don't
/// have to remember the dispatch sentinel.
pub async fn send_action(service: &str, action: &str, payload: serde_json::Value) -> Reply {
    let cfg = EnvConfig::load();
    let body = serde_json::json!({
        "action": action,
        "payload": payload,
    });
    send(
        service,
        crate::ipc::actions::ACTION_DISPATCH_PATH,
        body,
        Duration::from_secs_f64(cfg.default_timeout),
    )
    .await
}

/// Open a streaming action on `service` and return a [`Stream`] of chunks.
///
/// The returned stream yields `Ok(payload)` for each handler chunk and
/// `Err(IpcError)` for any stream-level or transport error. Heartbeat
/// frames (null payload, `done=false`) are silently consumed — they do
/// not appear as items.
///
/// **Cancellation:** dropping the stream drops the underlying pipe handle.
/// The server detects the close on its next chunk or heartbeat write,
/// aborts the handler task, and the handler's `sender.send().await` (or
/// `sender.closed().await`) resolves with an error. No further chunks
/// are produced.
///
/// **Errors:** connection failures surface as the first stream item
/// (`Err(IpcError { code: "pipe_connect", .. })`). Once that error is
/// observed, the stream returns `None` on the next poll. Mid-stream
/// errors (handler emits `Err`, peer disconnect, decode failure) end the
/// stream the same way.
pub fn send_action_stream(service: &str, action: &str, payload: serde_json::Value) -> IpcStream {
    let cfg = EnvConfig::load();
    let timeout = Duration::from_secs_f64(cfg.default_timeout);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<serde_json::Value, IpcError>>(16);

    if cfg.ipc_disable {
        let _ = tx.try_send(Err(IpcError::new(
            "ipc_disabled",
            "WYLDE_IPC_DISABLE is set; refusing to dispatch",
        )));
        return IpcStream::new(rx, None);
    }
    if cfg.transport != "pipe" {
        let _ = tx.try_send(Err(IpcError::new(
            "no_http_backend",
            "Rust ipc streaming has no HTTP fallback; set WYLDE_TRANSPORT=pipe",
        )));
        return IpcStream::new(rx, None);
    }

    let service = service.to_string();
    let action = action.to_string();
    let task = tokio::spawn(async move {
        run_stream_pipe(&service, &action, payload, timeout, &cfg, tx).await;
    });
    IpcStream::new(rx, Some(task))
}

/// Stream of chunks returned by [`send_action_stream`].
///
/// Implements [`futures::Stream<Item = Result<Value, IpcError>>`]. Dropping
/// the stream aborts the background reader task, which closes the pipe
/// handle — that's the signal the server uses to cancel the handler.
pub struct IpcStream {
    rx: tokio::sync::mpsc::Receiver<Result<serde_json::Value, IpcError>>,
    task: Option<tokio::task::JoinHandle<()>>,
    finished: bool,
}

impl IpcStream {
    fn new(
        rx: tokio::sync::mpsc::Receiver<Result<serde_json::Value, IpcError>>,
        task: Option<tokio::task::JoinHandle<()>>,
    ) -> Self {
        Self {
            rx,
            task,
            finished: false,
        }
    }
}

impl Stream for IpcStream {
    type Item = Result<serde_json::Value, IpcError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        match this.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.finished = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(Some(Ok(v))) => Poll::Ready(Some(Ok(v))),
        }
    }
}

impl Drop for IpcStream {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Like [`send_action`] but treats a non-ok reply as an error and returns
/// the inner data on success. Failed replies are surfaced as the same
/// `IpcError` value the server emitted.
pub async fn call_action(
    service: &str,
    action: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value, IpcError> {
    let reply = send_action(service, action, payload).await;
    if reply.ok {
        Ok(reply.data)
    } else {
        Err(reply
            .error
            .unwrap_or_else(|| IpcError::new("unknown", "ipc call failed with no error body")))
    }
}

// ── Pipe transport ────────────────────────────────────────────────────

#[cfg(windows)]
async fn send_pipe(
    service: &str,
    method: &str,
    http_verb: &str,
    data: &serde_json::Value,
    timeout: Duration,
    cfg: &EnvConfig,
) -> Reply {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::timeout as tk_timeout;

    let path = pipe_name(service);

    // Connect with retry on ERROR_PIPE_BUSY, matching Python's WaitNamedPipe loop.
    let connect_deadline =
        Instant::now() + Duration::from_millis(crate::ipc::wire::PIPE_CONNECT_TIMEOUT_MS);
    let mut client = loop {
        match ClientOptions::new().open(&path) {
            Ok(c) => break c,
            Err(e) if Instant::now() >= connect_deadline => {
                return Reply::err(IpcError::new(
                    "pipe_connect",
                    format!("connect({path}) failed: {e}"),
                ));
            }
            Err(_e) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };

    let handshake_timeout = Duration::from_secs_f64(cfg.handshake_timeout);

    // ── Handshake ────────────────────────────────────────────────────
    let handshake = HandshakeFrame {
        wylde_ipc: IPC_VERSION,
        caller: &cfg.self_name,
        service,
    };
    let hs_bytes = match rmp_serde::to_vec_named(&handshake) {
        Ok(b) => b,
        Err(e) => {
            return Reply::err(IpcError::new(
                "encode",
                format!("handshake encode failed: {e}"),
            ));
        }
    };

    let hs_write = async {
        let header = (hs_bytes.len() as u32).to_be_bytes();
        client.write_all(&header).await?;
        client.write_all(&hs_bytes).await?;
        client.flush().await?;
        Ok::<_, std::io::Error>(())
    };
    if let Err(e) = tk_timeout(handshake_timeout, hs_write).await {
        return Reply::err(IpcError::new("handshake_timeout", e.to_string()));
    }

    let hs_read = async {
        let mut header = [0u8; 4];
        client.read_exact(&mut header).await?;
        let n = u32::from_be_bytes(header) as usize;
        if n == 0 || n > crate::ipc::wire::MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("handshake reply size out of range: {n}"),
            ));
        }
        let mut body = vec![0u8; n];
        client.read_exact(&mut body).await?;
        Ok::<_, std::io::Error>(body)
    };
    let hs_body = match tk_timeout(handshake_timeout, hs_read).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            return Reply::err(IpcError::new("handshake_io", e.to_string()));
        }
        Err(e) => {
            return Reply::err(IpcError::new("handshake_timeout", e.to_string()));
        }
    };
    let hs_resp: HandshakeResponse = match rmp_serde::from_slice(&hs_body) {
        Ok(r) => r,
        Err(e) => {
            return Reply::err(IpcError::new(
                "decode",
                format!("handshake decode failed: {e}"),
            ));
        }
    };
    if hs_resp.wylde_ipc.is_some() && hs_resp.ok == Some(false) {
        let err = hs_resp
            .error
            .unwrap_or_else(|| IpcError::new("handshake_rejected", "handshake rejected"));
        return Reply::err(err);
    }
    // Pre-v1 server is fine; just continue.

    // ── Request frame ────────────────────────────────────────────────
    let path_str = if method.starts_with('/') {
        method.to_string()
    } else {
        format!("/{method}")
    };
    let req_id = Uuid::new_v4().simple().to_string();
    let envelope = RequestEnvelope {
        ver: IPC_VERSION,
        id: &req_id,
        method: &path_str,
        http_verb,
        data,
        meta: RequestMeta {
            deadline_ms: timeout.as_millis() as u64,
            caller: &cfg.self_name,
        },
    };
    let req_bytes = match rmp_serde::to_vec_named(&envelope) {
        Ok(b) => b,
        Err(e) => {
            return Reply::err(IpcError::new(
                "encode",
                format!("request encode failed: {e}"),
            ));
        }
    };

    let req_write = async {
        let header = (req_bytes.len() as u32).to_be_bytes();
        client.write_all(&header).await?;
        client.write_all(&req_bytes).await?;
        client.flush().await?;
        Ok::<_, std::io::Error>(())
    };
    if let Err(e) = tk_timeout(timeout, req_write).await {
        return Reply::err(IpcError::new("pipe_timeout", e.to_string()));
    }

    let reply_read = async {
        let mut header = [0u8; 4];
        client.read_exact(&mut header).await?;
        let n = u32::from_be_bytes(header) as usize;
        if n == 0 || n > crate::ipc::wire::MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("reply size out of range: {n}"),
            ));
        }
        let mut body = vec![0u8; n];
        client.read_exact(&mut body).await?;
        Ok::<_, std::io::Error>(body)
    };
    let reply_body = match tk_timeout(timeout, reply_read).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            return Reply::err(IpcError::new("pipe_io", e.to_string()));
        }
        Err(e) => {
            return Reply::err(IpcError::new("pipe_timeout", e.to_string()));
        }
    };
    let wire: WireReply = match rmp_serde::from_slice(&reply_body) {
        Ok(r) => r,
        Err(e) => {
            return Reply::err(IpcError::new("decode", format!("reply decode failed: {e}")));
        }
    };
    if wire.ok {
        Reply::ok(wire.data)
    } else {
        Reply::err(
            wire.error
                .unwrap_or_else(|| IpcError::new("unknown", "unknown error")),
        )
    }
}

#[cfg(not(windows))]
async fn send_pipe(
    _service: &str,
    _method: &str,
    _http_verb: &str,
    _data: &serde_json::Value,
    _timeout: Duration,
    _cfg: &EnvConfig,
) -> Reply {
    Reply::err(IpcError::new(
        "pipe_unavailable",
        "Windows named pipes not available on this platform",
    ))
}

// ── Streaming pipe driver ─────────────────────────────────────────────

#[cfg(windows)]
async fn run_stream_pipe(
    service: &str,
    action: &str,
    payload: serde_json::Value,
    timeout: Duration,
    cfg: &EnvConfig,
    tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, IpcError>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::timeout as tk_timeout;

    let path = pipe_name(service);

    // Connect (same retry semantics as send_pipe).
    let connect_deadline =
        Instant::now() + Duration::from_millis(crate::ipc::wire::PIPE_CONNECT_TIMEOUT_MS);
    let mut client = loop {
        match ClientOptions::new().open(&path) {
            Ok(c) => break c,
            Err(e) if Instant::now() >= connect_deadline => {
                let _ = tx
                    .send(Err(IpcError::new(
                        "pipe_connect",
                        format!("connect({path}) failed: {e}"),
                    )))
                    .await;
                return;
            }
            Err(_e) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };

    let handshake_timeout = Duration::from_secs_f64(cfg.handshake_timeout);

    // Handshake (identical to send_pipe).
    let handshake = HandshakeFrame {
        wylde_ipc: IPC_VERSION,
        caller: &cfg.self_name,
        service,
    };
    let hs_bytes = match rmp_serde::to_vec_named(&handshake) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx
                .send(Err(IpcError::new(
                    "encode",
                    format!("handshake encode failed: {e}"),
                )))
                .await;
            return;
        }
    };
    if let Err(e) = tk_timeout(handshake_timeout, async {
        let header = (hs_bytes.len() as u32).to_be_bytes();
        client.write_all(&header).await?;
        client.write_all(&hs_bytes).await?;
        client.flush().await?;
        Ok::<_, std::io::Error>(())
    })
    .await
    {
        let _ = tx
            .send(Err(IpcError::new("handshake_timeout", e.to_string())))
            .await;
        return;
    }
    let hs_body = match tk_timeout(handshake_timeout, async {
        let mut header = [0u8; 4];
        client.read_exact(&mut header).await?;
        let n = u32::from_be_bytes(header) as usize;
        if n == 0 || n > crate::ipc::wire::MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("handshake reply size out of range: {n}"),
            ));
        }
        let mut body = vec![0u8; n];
        client.read_exact(&mut body).await?;
        Ok::<_, std::io::Error>(body)
    })
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            let _ = tx
                .send(Err(IpcError::new("handshake_io", e.to_string())))
                .await;
            return;
        }
        Err(e) => {
            let _ = tx
                .send(Err(IpcError::new("handshake_timeout", e.to_string())))
                .await;
            return;
        }
    };
    let hs_resp: HandshakeResponse = match rmp_serde::from_slice(&hs_body) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx
                .send(Err(IpcError::new(
                    "decode",
                    format!("handshake decode failed: {e}"),
                )))
                .await;
            return;
        }
    };
    if hs_resp.wylde_ipc.is_some() && hs_resp.ok == Some(false) {
        let err = hs_resp
            .error
            .unwrap_or_else(|| IpcError::new("handshake_rejected", "handshake rejected"));
        let _ = tx.send(Err(err)).await; // wylde-check: discard-result-ok
        return;
    }

    // Request: same envelope as send_pipe but the action body carries
    // `stream: true` so the server takes the streaming dispatch path.
    let req_id = Uuid::new_v4().simple().to_string();
    let action_body = serde_json::json!({
        "action": action,
        "payload": payload,
        "stream": true,
    });
    let envelope = RequestEnvelope {
        ver: IPC_VERSION,
        id: &req_id,
        method: crate::ipc::actions::ACTION_DISPATCH_PATH,
        http_verb: "POST",
        data: &action_body,
        meta: RequestMeta {
            deadline_ms: timeout.as_millis() as u64,
            caller: &cfg.self_name,
        },
    };
    let req_bytes = match rmp_serde::to_vec_named(&envelope) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx
                .send(Err(IpcError::new(
                    "encode",
                    format!("request encode failed: {e}"),
                )))
                .await;
            return;
        }
    };
    if let Err(e) = tk_timeout(timeout, async {
        let header = (req_bytes.len() as u32).to_be_bytes();
        client.write_all(&header).await?;
        client.write_all(&req_bytes).await?;
        client.flush().await?;
        Ok::<_, std::io::Error>(())
    })
    .await
    {
        let _ = tx
            .send(Err(IpcError::new("pipe_timeout", e.to_string())))
            .await;
        return;
    }

    // Read chunks until done=true OR the consumer drops the receiver.
    let idle = Duration::from_secs_f64(cfg.idle_read_timeout);
    loop {
        // Pause reading whenever the receiver is full / closed: if the
        // consumer dropped the stream, `tx.send` below will fail and we
        // exit. The pipe handle's drop is what signals cancellation to
        // the server, so just letting this task return is enough.
        let read_res = tk_timeout(idle, async {
            let mut header = [0u8; 4];
            client.read_exact(&mut header).await?;
            let n = u32::from_be_bytes(header) as usize;
            if n == 0 || n > crate::ipc::wire::MAX_FRAME_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("chunk size out of range: {n}"),
                ));
            }
            let mut body = vec![0u8; n];
            client.read_exact(&mut body).await?;
            Ok::<_, std::io::Error>(body)
        })
        .await;
        let body = match read_res {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                let _ = tx.send(Err(IpcError::new("pipe_io", e.to_string()))).await; // subscriber channel may be dropped (wylde-check: discard-result-ok)
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(Err(IpcError::new("read_timeout", e.to_string())))
                    .await;
                return;
            }
        };
        let frame: ChunkFrame = match rmp_serde::from_slice(&body) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx
                    .send(Err(IpcError::new(
                        "decode",
                        format!("chunk decode failed: {e}"),
                    )))
                    .await;
                return;
            }
        };
        if !frame.ok {
            let err = frame
                .error
                .unwrap_or_else(|| IpcError::new("unknown", "stream error with no body"));
            let _ = tx.send(Err(err)).await; // wylde-check: discard-result-ok
            return;
        }
        // Two frame shapes carry no consumer-visible payload:
        //   • mid-stream heartbeat: done=false, payload=null
        //   • terminal "graceful end" frame: done=true, payload=null
        // The Phase-0 spec allows done=true to also carry a final payload
        // chunk; we yield that to the consumer, then end the stream.
        let suppress = frame.payload.is_null();
        if !suppress {
            // Yield the payload. If send fails the consumer is gone — quit
            // and let our drop close the pipe.
            if tx.send(Ok(frame.payload)).await.is_err() {
                return;
            }
        }
        if frame.done {
            return;
        }
    }
}

#[cfg(not(windows))]
async fn run_stream_pipe(
    _service: &str,
    _action: &str,
    _payload: serde_json::Value,
    _timeout: Duration,
    _cfg: &EnvConfig,
    tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, IpcError>>,
) {
    let _ = tx
        .send(Err(IpcError::new(
            "pipe_unavailable",
            "Windows named pipes not available on this platform",
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_handler_records_path() {
        // Use a unique method name so this test can run in parallel with itself.
        register_handler("test_method_xyz", Some("/custom/path"));
        assert_eq!(resolve_handler_path("test_method_xyz"), "/custom/path");
    }

    #[test]
    fn unregistered_method_falls_back_to_slash_prefix() {
        assert_eq!(
            resolve_handler_path("never_registered_zzz"),
            "/never_registered_zzz"
        );
    }

    #[test]
    fn unregistered_method_with_leading_slash_passes_through() {
        assert_eq!(resolve_handler_path("/already/slashed"), "/already/slashed");
    }

    #[tokio::test]
    async fn connection_failure_returns_clean_reply() {
        // Pipe is guaranteed not to exist (random suffix).
        let svc = format!("ipc-test-missing-{}", uuid::Uuid::new_v4().simple());
        let reply = send(
            &svc,
            "ping",
            serde_json::Value::Null,
            Duration::from_millis(500),
        )
        .await;
        assert!(!reply.ok, "expected error reply, got: {reply:?}");
        let err = reply.error.expect("error body");
        // On Windows we hit pipe_connect; on non-Windows we hit pipe_unavailable.
        assert!(
            err.code == "pipe_connect" || err.code == "pipe_unavailable",
            "unexpected error code: {}",
            err.code
        );
    }

    #[tokio::test]
    async fn timeout_is_respected() {
        // Try to talk to a non-existent service with a very short timeout. The
        // call should complete (with an error) well within a reasonable budget.
        let svc = format!("ipc-test-timeout-{}", uuid::Uuid::new_v4().simple());
        let start = Instant::now();
        let _ = send(
            &svc,
            "x",
            serde_json::Value::Null,
            Duration::from_millis(100),
        )
        .await;
        // The connect deadline is 2s; we should not exceed ~3s here.
        assert!(start.elapsed() < Duration::from_secs(4));
    }
}
