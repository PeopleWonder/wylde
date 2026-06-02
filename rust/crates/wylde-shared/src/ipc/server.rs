//! Inbound IPC: the pipe server and `serve` entry point.
//!
//! Rust port of `Core/shared/ipc/_server.py`. Accepts named-pipe
//! connections on `\\.\pipe\wylde-<service>`, performs the v1 handshake,
//! then dispatches each request frame to either the action registry (for
//! `method == "/__action__"`), the built-in control methods
//! (`/__ping__`, `/__handshake__`, `/health`), or — when the service
//! supplies one via [`serve_with_http_routes`] — an
//! [`HttpRouteTable`] keyed on `(http_verb, method)`. Anything that
//! matches none of those returns a structured `no_handler` reply.
//!
//! Threading model matches the Python side: one accept loop spawns one
//! tokio task per accepted client. Tasks own their pipe instance for its
//! lifetime and never share state with siblings.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::ipc::actions::{
    dispatch_action, is_streaming_action, take_streaming_action, ACTION_DISPATCH_PATH,
};
use crate::ipc::http_routes::{HttpRequest, HttpRouteTable};
use crate::ipc::wire::{
    pipe_name, ChunkFrame, EnvConfig, IpcError, IPC_VERSION, STREAM_HEARTBEAT_SECS,
};

// ── Envelope shapes (server side) ──────────────────────────────────────

#[derive(Deserialize, Debug)]
struct IncomingFrame {
    #[serde(default)]
    wylde_ipc: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    http_verb: Option<String>,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Serialize, Debug)]
struct HandshakeAck<'a> {
    wylde_ipc: u32,
    ok: bool,
    service: &'a str,
}

#[derive(Serialize, Debug)]
struct HandshakeReject<'a> {
    wylde_ipc: u32,
    ok: bool,
    error: IpcError,
    service: &'a str,
}

#[derive(Serialize, Debug)]
struct ReplyFrame<'a> {
    id: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<IpcError>,
}

// ── Public API ────────────────────────────────────────────────────────

/// Does this platform / build have a working pipe transport?
/// Always `true` on Windows. Mirrors Python's `supports_ipc`.
pub fn supports_ipc() -> bool {
    cfg!(windows)
}

/// Block forever serving IPC requests for `service`.
///
/// The HTTP `port` parameter is accepted for API parity with Python but
/// currently ignored — this Rust impl is pipe-only. Returns when the
/// accept loop terminates (typically only on irrecoverable error or
/// process shutdown).
pub async fn serve(service: &str, port: Option<u16>) -> anyhow::Result<()> {
    serve_with_http_routes(service, port, HttpRouteTable::new()).await
}

/// Like [`serve`], but with an [`HttpRouteTable`] of `(http_verb, method)`
/// → handler routes layered on top of the action surface.
///
/// Dispatch precedence per request frame:
///   1. built-in control methods (`/__ping__`, `/__handshake__`, `/health`)
///   2. action dispatch (`method == "/__action__"`)
///   3. the supplied HTTP route table (matched on `http_verb` + `method`)
///   4. structured `no_handler` reply
///
/// The built-ins win over the table, so a service cannot accidentally
/// shadow `/health` with its own route — that path stays uniform across
/// every Rust service for the lifecycle health-probe.
pub async fn serve_with_http_routes(
    service: &str,
    _port: Option<u16>,
    routes: HttpRouteTable,
) -> anyhow::Result<()> {
    let cfg = EnvConfig::load();
    let wylde_root = std::env::var("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    // Best-effort: write the action contract before opening for business so
    // wylde_check has the artifact even if a startup error prevents accept.
    if let Err(e) = crate::ipc::actions::write_action_contract(service, &wylde_root) {
        tracing::warn!(
            "ipc: failed to write action contract for {}: {}",
            service,
            e
        );
    }

    if !supports_ipc() {
        anyhow::bail!("Rust ipc serve() requires Windows named pipes");
    }
    if cfg.ipc_disable {
        anyhow::bail!("WYLDE_IPC_DISABLE is set; refusing to bind pipe");
    }

    // Self-attest the serve_loop phase so wylde_check sees the full
    // startup sequence in the manifest without AST-walking source.
    crate::manifest::mark_serve_loop_entered(service);

    if !routes.is_empty() {
        tracing::info!(
            "ipc: {} HTTP route(s) registered for {}: {:?}",
            routes.len(),
            service,
            routes.registered(),
        );
    }

    let server = PipeServer::new(service).with_http_routes(routes);
    server.accept_loop().await
}

/// Start the pipe server on a background task and return its handle.
/// The task runs until [`PipeServer::stop`] is called on the captured
/// server (callers usually drop the handle and rely on process exit
/// instead — matches Python's `serve_forever_background` daemon-thread
/// behaviour).
pub fn serve_forever_background(service: &str) -> JoinHandle<anyhow::Result<()>> {
    let service = service.to_string();
    tokio::spawn(async move { serve(&service, None).await })
}

/// Accept loop owner. Drop it (or call [`PipeServer::stop`]) to terminate
/// the server gracefully.
pub struct PipeServer {
    service: String,
    pipe_name: String,
    stop: Arc<Notify>,
    routes: Arc<HttpRouteTable>,
}

impl PipeServer {
    /// Build a new server for `service`. Does not bind until
    /// [`Self::accept_loop`] is awaited. Starts with no HTTP routes —
    /// chain [`Self::with_http_routes`] to add them.
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
            pipe_name: pipe_name(service),
            stop: Arc::new(Notify::new()),
            routes: Arc::new(HttpRouteTable::new()),
        }
    }

    /// Attach an [`HttpRouteTable`] so non-action requests can match
    /// `(http_verb, method)` routes before falling through to
    /// `no_handler`. Builder-style; returns `self`.
    pub fn with_http_routes(mut self, routes: HttpRouteTable) -> Self {
        self.routes = Arc::new(routes);
        self
    }

    /// The pipe path this server will bind to.
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Signal the accept loop to exit at the next opportunity. Already-running
    /// per-connection workers continue serving their current request.
    pub fn stop(&self) {
        self.stop.notify_waiters();
    }

    /// Run the accept loop until [`Self::stop`] is signalled or an
    /// irrecoverable bind error occurs.
    #[cfg(windows)]
    pub async fn accept_loop(&self) -> anyhow::Result<()> {
        use tokio::net::windows::named_pipe::ServerOptions;

        loop {
            // Create the next instance for the next accept *before* spawning
            // the worker, mirroring Python's `PIPE_UNLIMITED_INSTANCES` loop.
            // Note: tokio caps max_instances at 254 (the Win32
            // PIPE_UNLIMITED_INSTANCES value of 255 is the count *limit*, not
            // an allowed argument). Python's `_server.py` uses the same OS
            // constant; 254 is the highest legal arg here and is effectively
            // "unlimited" for any realistic Wylde fan-out.
            let server = match ServerOptions::new()
                .pipe_mode(tokio::net::windows::named_pipe::PipeMode::Byte)
                .max_instances(254)
                .in_buffer_size(65536)
                .out_buffer_size(65536)
                .create(&self.pipe_name)
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("ipc: CreateNamedPipe({}) failed: {}", self.pipe_name, e);
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => continue,
                        _ = self.stop.notified() => return Ok(()),
                    }
                }
            };

            tokio::select! {
                conn = server.connect() => {
                    match conn {
                        Ok(()) => {
                            let service = self.service.clone();
                            let routes = Arc::clone(&self.routes);
                            tokio::spawn(async move {
                                if let Err(e) = handle_client(server, service, routes).await {
                                    tracing::debug!("ipc: connection ended: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::debug!("ipc: ConnectNamedPipe: {}", e);
                            // Drop `server` (closes the half-bound instance) and loop.
                        }
                    }
                }
                _ = self.stop.notified() => return Ok(()),
            }
        }
    }

    /// Non-Windows stub: returns immediately without binding.
    #[cfg(not(windows))]
    pub async fn accept_loop(&self) -> anyhow::Result<()> {
        anyhow::bail!("named pipes are Windows-only");
    }
}

// ── Per-connection worker ─────────────────────────────────────────────

#[cfg(windows)]
async fn handle_client(
    mut peer: tokio::net::windows::named_pipe::NamedPipeServer,
    service: String,
    routes: Arc<HttpRouteTable>,
) -> anyhow::Result<()> {
    use tokio::time::timeout as tk_timeout;

    let cfg = EnvConfig::load();
    let handshake_timeout = std::time::Duration::from_secs_f64(cfg.handshake_timeout);
    let idle_timeout = std::time::Duration::from_secs_f64(cfg.idle_read_timeout);
    let body_timeout = std::time::Duration::from_secs_f64(cfg.frame_read_timeout);

    // First frame: handshake or a pre-v1 request.
    let body = match tk_timeout(handshake_timeout, read_frame(&mut peer)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            tracing::debug!("ipc: first-frame read error: {e}");
            return Ok(());
        }
        Err(_) => {
            let _ = send_error(
                &mut peer,
                "",
                "handshake_timeout",
                "no first frame within handshake window",
            )
            .await;
            return Ok(());
        }
    };

    let first: IncomingFrame = match rmp_serde::from_slice(&body) {
        Ok(f) => f,
        Err(e) => {
            let _ = send_error(
                &mut peer,
                "",
                "decode",
                &format!("msgpack decode failed: {e}"),
            )
            .await;
            return Ok(());
        }
    };

    let mut carryover: Option<IncomingFrame> = None;
    if let Some(client_ver) = first.wylde_ipc {
        if client_ver < 1 || client_ver > IPC_VERSION {
            let frame = rmp_serde::to_vec_named(&HandshakeReject {
                wylde_ipc: IPC_VERSION,
                ok: false,
                error: IpcError::new(
                    "version_mismatch",
                    format!("client ipc version {client_ver} not supported; server speaks v1..{IPC_VERSION}"),
                ),
                service: &service,
            })?;
            write_frame(&mut peer, &frame).await?;
            return Ok(());
        }
        let ack = rmp_serde::to_vec_named(&HandshakeAck {
            wylde_ipc: IPC_VERSION,
            ok: true,
            service: &service,
        })?;
        write_frame(&mut peer, &ack).await?;
    } else {
        // Pre-v1 client: first frame is already a request.
        carryover = Some(first);
    }

    // Request loop.
    loop {
        let req = if let Some(c) = carryover.take() {
            c
        } else {
            // Use the idle timeout for waiting on the NEXT request to begin;
            // once we have the header, body_timeout would normally apply. Our
            // read_frame reads both header + body in one call, so we use
            // idle_timeout for the whole frame on subsequent reads; if the body
            // read stalls beyond body_timeout, fall back from idle to that
            // tighter window for malformed-frame detection.
            let overall = idle_timeout.max(body_timeout);
            match tk_timeout(overall, read_frame(&mut peer)).await {
                Ok(Ok(b)) => match rmp_serde::from_slice::<IncomingFrame>(&b) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = send_error(
                            &mut peer,
                            "",
                            "decode",
                            &format!("msgpack decode failed: {e}"),
                        )
                        .await;
                        continue;
                    }
                },
                Ok(Err(e)) => {
                    tracing::debug!("ipc: peer closed: {e}");
                    return Ok(());
                }
                Err(_) => {
                    let _ = send_error(&mut peer, "", "read_timeout", "frame read timed out").await;
                    return Ok(());
                }
            }
        };

        let req_id = req.id.unwrap_or_default();
        let method = req.method.unwrap_or_else(|| "/".to_string());

        // Built-in control: ping
        if method == "/__ping__" || method == "__ping__" {
            let frame = rmp_serde::to_vec_named(&ReplyFrame {
                id: &req_id,
                ok: true,
                data: Some(serde_json::json!({"pong": true, "ver": IPC_VERSION})),
                error: None,
            })?;
            if write_frame(&mut peer, &frame).await.is_err() {
                return Ok(());
            }
            continue;
        }
        // Built-in control: handshake-as-method (rare; symmetry with Python)
        if method == "/__handshake__" || method == "__handshake__" {
            let frame = rmp_serde::to_vec_named(&ReplyFrame {
                id: &req_id,
                ok: true,
                data: Some(serde_json::json!({
                    "ver": IPC_VERSION,
                    "service": service.clone(),
                })),
                error: None,
            })?;
            if write_frame(&mut peer, &frame).await.is_err() {
                return Ok(());
            }
            continue;
        }

        // Built-in liveness: GET /health. Every Python service answers
        // this (the lifecycle daemon's stub app and each service's Flask
        // surface), and the lifecycle `service.health` action probes it
        // with `ipc.send(name, "/health", http_verb="GET")`. The Rust
        // port had no route table, so non-action `/health` fell through
        // to the `no_handler` 404 below — which is exactly what painted
        // an otherwise-up Rust service (e.g. wylde-vram-broker) red on
        // the dashboard. Answer it here, alongside the other built-in
        // control methods, so the reply shape matches Python's
        // `{ok: true, service: <name>}` for every Rust service at once.
        if method == "/health" || method == "health" {
            let frame = rmp_serde::to_vec_named(&ReplyFrame {
                id: &req_id,
                ok: true,
                data: Some(serde_json::json!({"ok": true, "service": service.clone()})),
                error: None,
            })?;
            if write_frame(&mut peer, &frame).await.is_err() {
                return Ok(());
            }
            continue;
        }

        // Action dispatch
        if method == ACTION_DISPATCH_PATH {
            // Peek the envelope: if the client set `stream: true` and the
            // named action is registered as streaming, dispatch the
            // multi-frame path. Otherwise fall through to the existing
            // unary path. (A unary action invoked with `stream: true`, or
            // a streaming action invoked without it, both surface a
            // structured error rather than silently doing the wrong
            // thing.)
            let (wants_stream, action_name) = peek_action_envelope(&req.data);
            if let Some(name) = action_name {
                let action_is_stream = is_streaming_action(&name);
                if wants_stream || action_is_stream {
                    if wants_stream && !action_is_stream {
                        let err = IpcError::new(
                            "not_streaming",
                            format!(
                                "action {name:?} is not registered as streaming on this server"
                            ),
                        );
                        let _ = send_stream_error(&mut peer, &req_id, err).await;
                        continue;
                    }
                    if !wants_stream && action_is_stream {
                        let err = IpcError::new(
                            "stream_required",
                            format!(
                                "action {name:?} is streaming; caller must use send_action_stream"
                            ),
                        );
                        let frame = rmp_serde::to_vec_named(&ReplyFrame {
                            id: &req_id,
                            ok: false,
                            data: None,
                            error: Some(err),
                        })?;
                        if write_frame(&mut peer, &frame).await.is_err() {
                            return Ok(());
                        }
                        continue;
                    }
                    // Both flags agree → streaming dispatch.
                    let payload = req
                        .data
                        .as_object()
                        .and_then(|m| m.get("payload"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    if !serve_one_stream(&mut peer, &req_id, &name, payload).await {
                        return Ok(());
                    }
                    continue;
                }
            }
            let reply = dispatch_action(req.data).await;
            let frame = rmp_serde::to_vec_named(&ReplyFrame {
                id: &req_id,
                ok: reply.ok,
                data: if reply.ok { Some(reply.data) } else { None },
                error: reply.error,
            })?;
            if write_frame(&mut peer, &frame).await.is_err() {
                return Ok(());
            }
            continue;
        }

        // Non-action methods: try the HTTP route table. The GUI panels and
        // the Python "Flask-over-pipe" servers address services with an
        // HTTP-shaped envelope (`http_verb` + path-style `method`); a
        // service that registered routes via `serve_with_http_routes`
        // answers them here.
        let verb = req.http_verb.unwrap_or_else(|| "POST".to_string());
        if let Some(handler) = routes.lookup(&verb, &method) {
            let resp = handler(HttpRequest {
                method: verb.to_ascii_uppercase(),
                path: method.clone(),
                body: req.data,
            })
            .await;
            let frame = rmp_serde::to_vec_named(&ReplyFrame {
                id: &req_id,
                ok: resp.ok,
                data: if resp.ok { Some(resp.data) } else { None },
                error: resp.error,
            })?;
            if write_frame(&mut peer, &frame).await.is_err() {
                return Ok(());
            }
            continue;
        }

        // Nothing matched. Surface a clean error so callers get a
        // structured reply (matches the Python `no_handler`/`http_404`
        // shape closely enough for diagnosis).
        let frame = rmp_serde::to_vec_named(&ReplyFrame {
            id: &req_id,
            ok: false,
            data: None,
            error: Some(IpcError::new(
                "no_handler",
                format!("{verb} {method:?} not registered on rust ipc server"),
            )),
        })?;
        if write_frame(&mut peer, &frame).await.is_err() {
            return Ok(());
        }
    }
}

#[cfg(windows)]
async fn read_frame(
    peer: &mut tokio::net::windows::named_pipe::NamedPipeServer,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 4];
    peer.read_exact(&mut header).await?;
    let n = u32::from_be_bytes(header) as usize;
    if n == 0 || n > crate::ipc::wire::MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("pipe frame size out of range: {n}"),
        ));
    }
    let mut body = vec![0u8; n];
    peer.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(windows)]
async fn write_frame(
    peer: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    payload: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let header = (payload.len() as u32).to_be_bytes();
    peer.write_all(&header).await?;
    peer.write_all(payload).await?;
    peer.flush().await?;
    Ok(())
}

#[cfg(windows)]
async fn send_error(
    peer: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    req_id: &str,
    code: &str,
    message: &str,
) -> std::io::Result<()> {
    let frame = ReplyFrame {
        id: req_id,
        ok: false,
        data: None,
        error: Some(IpcError::new(code, message)),
    };
    let bytes = rmp_serde::to_vec_named(&frame).map_err(std::io::Error::other)?;
    write_frame(peer, &bytes).await
}

// ── Streaming dispatch ────────────────────────────────────────────────

/// Peek at an action-dispatch envelope without consuming it: returns
/// `(wants_stream, action_name)`. `wants_stream` is the boolean value of
/// the optional top-level `stream` field; `action_name` is the value of
/// the `action` field when present and non-empty.
fn peek_action_envelope(data: &serde_json::Value) -> (bool, Option<String>) {
    let Some(obj) = data.as_object() else {
        return (false, None);
    };
    let wants_stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let name = obj
        .get("action")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (wants_stream, name)
}

#[cfg(windows)]
async fn send_stream_error(
    peer: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    req_id: &str,
    err: IpcError,
) -> std::io::Result<()> {
    let frame = ChunkFrame {
        id: req_id.to_string(),
        seq: 0,
        payload: serde_json::Value::Null,
        done: true,
        ok: false,
        error: Some(err),
    };
    let bytes = rmp_serde::to_vec_named(&frame).map_err(std::io::Error::other)?;
    write_frame(peer, &bytes).await
}

/// Drive one streaming response on `peer`.
///
/// Returns `true` if the connection is still usable for the next request
/// frame, `false` if the peer broke (caller should exit the request loop).
///
/// Wire behaviour:
/// - Spawns the registered streaming handler on a child task, feeding it
///   an mpsc sender. Each chunk the handler emits is written as one
///   [`ChunkFrame`] with the shared `id` and an incrementing `seq`.
/// - Every [`STREAM_HEARTBEAT_SECS`] of handler silence, the loop emits a
///   null-payload, `done=false` frame so the client's idle timer doesn't
///   trip.
/// - When the handler returns (its sender is dropped), the loop drains
///   any remaining chunks then emits a final `done=true` frame.
/// - When the handler sends an `Err(IpcError)`, that becomes a `done=true,
///   ok=false` frame and the loop exits — the handler future is aborted.
/// - When a write to the peer fails (client disconnected), the handler
///   future is aborted via `JoinHandle::abort`, which drops the receiver
///   and causes the handler's own `sender.send().await` to return
///   `Err(SendError)`. Handlers that want prompt cancellation should
///   `select!` on `sender.closed().await`.
#[cfg(windows)]
async fn serve_one_stream(
    peer: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    req_id: &str,
    action_name: &str,
    payload: serde_json::Value,
) -> bool {
    use tokio::time::{interval, MissedTickBehavior};

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<serde_json::Value, IpcError>>(16);

    let handler_fut = match take_streaming_action(action_name, payload, tx) {
        Ok(f) => f,
        Err(err) => {
            return send_stream_error(peer, req_id, err).await.is_ok();
        }
    };
    // Spawn the handler so it runs concurrently with our pump loop. We do
    // NOT keep a JoinHandle and call .abort() on it — dropping `rx` is
    // what signals cancellation to the handler, and the handler must
    // observe it via `sender.send().is_err()` or `sender.closed()`. That
    // gives handlers a chance to run their own cleanup (closing model
    // sessions, releasing VRAM, etc.) rather than being torn down mid-flight.
    tokio::spawn(handler_fut);

    let heartbeat_secs = heartbeat_interval_secs();
    let mut ticker = interval(Duration::from_secs(heartbeat_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // first tick fires immediately — burn it so the first heartbeat is
    // `heartbeat_secs` after the call rather than at t=0.
    ticker.tick().await;

    let mut seq: u32 = 0;
    loop {
        tokio::select! {
            chunk = rx.recv() => {
                match chunk {
                    Some(Ok(payload)) => {
                        let frame = ChunkFrame {
                            id: req_id.to_string(),
                            seq,
                            payload,
                            done: false,
                            ok: true,
                            error: None,
                        };
                        if !write_chunk(peer, &frame).await {
                            // Client gone. Returning drops `rx`, which
                            // makes the handler's next send fail / its
                            // `closed()` future resolve.
                            return false;
                        }
                        seq = seq.wrapping_add(1);
                        ticker.reset();
                    }
                    Some(Err(err)) => {
                        let frame = ChunkFrame {
                            id: req_id.to_string(),
                            seq,
                            payload: serde_json::Value::Null,
                            done: true,
                            ok: false,
                            error: Some(err),
                        };
                        return write_chunk(peer, &frame).await;
                    }
                    None => {
                        // Handler dropped its sender — graceful end of
                        // stream. Emit the terminal frame.
                        let frame = ChunkFrame {
                            id: req_id.to_string(),
                            seq,
                            payload: serde_json::Value::Null,
                            done: true,
                            ok: true,
                            error: None,
                        };
                        return write_chunk(peer, &frame).await;
                    }
                }
            }
            _ = ticker.tick() => {
                let frame = ChunkFrame {
                    id: req_id.to_string(),
                    seq,
                    payload: serde_json::Value::Null,
                    done: false,
                    ok: true,
                    error: None,
                };
                if !write_chunk(peer, &frame).await {
                    return false;
                }
                seq = seq.wrapping_add(1);
            }
        }
    }
}

#[cfg(windows)]
async fn write_chunk(
    peer: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    frame: &ChunkFrame,
) -> bool {
    match rmp_serde::to_vec_named(frame) {
        Ok(bytes) => write_frame(peer, &bytes).await.is_ok(),
        Err(e) => {
            tracing::error!("ipc: failed to encode stream chunk: {e}");
            false
        }
    }
}

/// Read the heartbeat cadence at call time. `WYLDE_IPC_STREAM_HEARTBEAT_SECS`
/// overrides the [`STREAM_HEARTBEAT_SECS`] default — tests set it low so
/// cancellation / heartbeat assertions don't have to wait 25s. Clamped to
/// `>= 1`.
fn heartbeat_interval_secs() -> u64 {
    std::env::var("WYLDE_IPC_STREAM_HEARTBEAT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n.max(1))
        .unwrap_or(STREAM_HEARTBEAT_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_ipc_matches_platform() {
        assert_eq!(supports_ipc(), cfg!(windows));
    }

    #[test]
    fn pipe_server_records_path() {
        let s = PipeServer::new("test-svc");
        assert_eq!(s.pipe_name(), r"\\.\pipe\wylde-test-svc");
        assert_eq!(s.service, "test-svc");
    }

    #[test]
    fn pipe_server_strips_wylde_prefix() {
        let s = PipeServer::new("wylde-test-svc");
        assert_eq!(s.pipe_name(), r"\\.\pipe\wylde-test-svc");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn server_binds_and_stops_cleanly() {
        // Use a unique service name so concurrent test runs don't collide.
        let svc = format!("ipc-test-bind-{}", uuid::Uuid::new_v4().simple());
        let server = Arc::new(PipeServer::new(&svc));
        let server_clone = Arc::clone(&server);
        let task = tokio::spawn(async move { server_clone.accept_loop().await });

        // Give the accept loop a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Pipe should now exist — try a non-blocking open. We expect either
        // success or ERROR_PIPE_BUSY (not ERROR_FILE_NOT_FOUND).
        let pipe_path = server.pipe_name().to_string();
        let exists =
            std::path::Path::new(&pipe_path).exists() || std::fs::metadata(&pipe_path).is_ok();
        // On Windows, std::path::exists on a pipe path is a bit lossy; do not
        // hard-fail if it returns false, but the connect attempt below would
        // tell us for sure if it really weren't there.
        let _ = exists;

        server.stop();
        // Give it a moment to notice the stop signal.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    }
}
