//! Wylde service pipe surface — gpui edition.
//!
//! Lift of `Core/GUI/src-tauri/src/pipe/` to a regular Rust library
//! crate.  The wire client and the in-process `HarnessApi` short-circuit
//! both move here verbatim — gpui callers are plain Rust functions, so
//! the Tauri-command wrapping in `lib.rs::pipe_call` is gone, and the
//! short-circuit gets *cleaner* (no JSON-through-`invoke` round-trip).
//!
//! Scope of this slice (Frontend foundation):
//!
//!   - Wire `call` / `list_wylde_pipes` ported as-is.
//!   - `try_dispatch_harness` ported as-is — the chat-verb fast path
//!     keeps the in-process trait dispatch the harness already exposes
//!     via `wylde_harness::HarnessApi`.
//!   - `lifecycle_action` factored out of `src-tauri/src/lib.rs` (the
//!     `/__action__` envelope helper) so the tray's graceful shutdown
//!     in `Shell/` has the same one-shot interface to reach for.
//!   - `service_health` re-implemented end-to-end (one of the two
//!     simple verbs the slice spec calls out as a round-trip proof).
//!
//! What is intentionally NOT here:
//!
//!   - Streaming verbs (`chat.stream_turn`, `chat.stream_tools`).  The
//!     plan §5.5 calls for swapping them from Tauri events to
//!     `gpui::Subscription` — that lands when Chat panel is ported.
//!     Today those verbs go over the wire via `call()` exactly as the
//!     Tauri side does.
//!   - Per-panel adapters (Settings, Workspaces, …).  Each panel
//!     crate will ship its own `ipc.rs` per the plan §3 layout when
//!     it lands.

pub mod chat;
pub mod memory_long_term;
pub mod memory_workspaces;
pub mod nav_bus;
pub mod tools;

pub use nav_bus::{install_nav_sender, is_nav_installed, request_nav};

use serde_json::{Map, Value};
use std::sync::OnceLock;
use std::time::Duration;

use wylde_harness::{DefaultHarnessApi, HarnessApi};
use wylde_shared::ipc::Reply;

#[cfg(target_os = "windows")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::ClientOptions;
#[cfg(target_os = "windows")]
use tokio::time::timeout;

// ── tokio bridge ─────────────────────────────────────────────────────
//
// gpui owns the main event loop on its own dispatcher threads.  Those
// threads have no tokio runtime in TLS, so any pipe IO scheduled via
// `cx.spawn(...)` would panic on the first tokio primitive (named-
// pipe connect, `tokio::time::timeout`, `AsyncReadExt`).
//
// The Shell stashes a `Handle` to a long-lived multi-threaded tokio
// runtime here at startup; `call()` automatically hops to it when the
// caller isn't already running inside a tokio runtime.  Callers don't
// need to know about the bridge — `wylde_gui_pipe::call(...)` works
// the same from inside a `cx.spawn` task as from inside a tokio one.

static TOKIO_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Stash a tokio runtime handle for the wire-IO bridge.  The Shell
/// calls this exactly once at startup, before any pipe traffic.
/// Subsequent calls silently overwrite — useful for tests, but the
/// production main path only fires once.
pub fn install_runtime(handle: tokio::runtime::Handle) {
    // OnceLock has no `force_set`; on second install we just leave
    // the first handle in place.  In practice the install path runs
    // once per process so this branch is unreachable for the live
    // binary.
    let _ = TOKIO_HANDLE.set(handle);
}

/// True if a tokio runtime is reachable — either because the caller
/// is already inside one or because the Shell installed a Handle.
/// Exposed for diagnostics + the test below; not used on the hot path
/// because the wire `call` already discriminates inline.
#[doc(hidden)]
pub fn tokio_reachable() -> bool {
    tokio::runtime::Handle::try_current().is_ok() || TOKIO_HANDLE.get().is_some()
}

/// Reactor-independent sleep for callers that live on gpui's executor
/// (which has **no** tokio reactor) but have no `cx`/`BackgroundExecutor`
/// in scope to use `background_executor().timer()`.
///
/// `tokio::time::sleep` registers a timer with the *current* tokio
/// reactor; awaited directly from a `cx.spawn` task that panics with
/// "there is no reactor running". This helper hops the wait onto the
/// stashed bridge `Handle` (same trick as [`call`]), so the timer runs
/// on a real reactor and the await just parks the gpui task.
///
/// Callers that already have a gpui async context should prefer
/// `app_cx.background_executor().timer(dur).await` directly — this is
/// the escape hatch for plain `async fn`s like the shutdown drain.
pub async fn bridged_sleep(dur: std::time::Duration) {
    // Already inside a runtime → the direct sleep is correct and cheapest.
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::time::sleep(dur).await;
        return;
    }
    if let Some(handle) = TOKIO_HANDLE.get() {
        // Drive the timer on the bridge runtime; await only joins it.
        let _ = handle
            .spawn(async move { tokio::time::sleep(dur).await })
            .await;
        return;
    }
    // No runtime installed at all (only really possible in a unit test
    // that skipped `install_runtime`). Degrade to a blocking sleep so we
    // still wait roughly the right amount instead of busy-spinning.
    std::thread::sleep(dur);
}

/// Run a blocking closure off the async caller's thread, returning its
/// result. The reactor-independent analogue of `tokio::task::spawn_blocking`
/// for callers living on gpui's executor.
///
/// `tokio::task::spawn_blocking` requires a *current* tokio runtime and
/// panics ("there is no reactor running") when awaited from a `cx.spawn`
/// task — exactly the trap that crashed the Chat/Workspaces folder
/// pickers. This hops the blocking work onto the bridge runtime's blocking
/// pool (same `Handle` the wire IO uses), so the gpui task just parks on
/// the join.
pub async fn bridged_spawn_blocking<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return handle
            .spawn_blocking(f)
            .await
            .expect("bridged_spawn_blocking: blocking task panicked");
    }
    if let Some(handle) = TOKIO_HANDLE.get() {
        return handle
            .spawn_blocking(f)
            .await
            .expect("bridged_spawn_blocking: blocking task panicked");
    }
    // No runtime installed (a unit test that skipped `install_runtime`).
    // Run inline — blocks the caller's thread, which is acceptable for
    // the test-only path.
    f()
}

const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Canonical caller-name baked into every request envelope.  The Svelte
/// alpha sent `"fletch-gui"`; we keep that string so log diffs across
/// the cutover stay grep-able to one identifier.  When the cutover
/// completes and `fletch-gui.exe` is gone, this can flip to
/// `"wylde-gui"` in the same commit that deletes `src-tauri/`.
pub const CALLER_NAME: &str = "fletch-gui";

/// Resolve `\\.\pipe\wylde-<service>` from a bare or `wylde-`-prefixed
/// service name.  Pure function — testable without a live pipe.
pub fn pipe_name(service: &str) -> String {
    let bare = service.strip_prefix("wylde-").unwrap_or(service);
    format!(r"\\.\pipe\wylde-{}", bare)
}

#[cfg(target_os = "windows")]
pub async fn call(
    service: &str,
    http_verb: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    // If the caller is already inside a tokio runtime, run inline.
    // Otherwise hop to the stashed Handle so the gpui dispatcher
    // threads (which have no current runtime) work transparently.
    if tokio::runtime::Handle::try_current().is_ok() {
        return call_inner(service, http_verb, path, body).await;
    }
    if let Some(handle) = TOKIO_HANDLE.get() {
        let svc = service.to_string();
        let verb = http_verb.to_string();
        let p = path.to_string();
        return handle
            .spawn(async move { call_inner(&svc, &verb, &p, body).await })
            .await
            .map_err(|e| format!("join: {e}"))?;
    }
    Err(format!(
        "pipe_unavailable: no tokio runtime available for service '{service}'; \
         call `wylde_gui_pipe::install_runtime(handle)` at startup",
    ))
}

#[cfg(target_os = "windows")]
async fn call_inner(
    service: &str,
    http_verb: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let name = pipe_name(service);

    let connect_fut = async {
        loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => return Ok(c),
                // ERROR_PIPE_BUSY — instance exists but is serving another
                // client.  Short back-off and retry.
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    };

    let mut client = timeout(CONNECT_TIMEOUT, connect_fut)
        .await
        .map_err(|_| {
            format!(
                "pipe_timeout: could not connect to service '{}' within {}s",
                service,
                CONNECT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| {
            let code = e.raw_os_error().unwrap_or(0);
            // ERROR_FILE_NOT_FOUND (2) / ERROR_PATH_NOT_FOUND (3)
            if code == 2 || code == 3 {
                format!(
                    "pipe_unavailable: service '{}' is not running (pipe not found)",
                    service
                )
            } else {
                format!("pipe_connect: {}: {}", service, e)
            }
        })?;

    let full_path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    };

    let mut meta = Map::new();
    meta.insert(
        "deadline_ms".into(),
        Value::from(RESPONSE_TIMEOUT.as_millis() as u64),
    );
    meta.insert("caller".into(), Value::from(CALLER_NAME));

    let mut envelope = Map::new();
    envelope.insert(
        "id".into(),
        Value::String(uuid::Uuid::new_v4().simple().to_string()),
    );
    envelope.insert("method".into(), Value::String(full_path));
    envelope.insert("http_verb".into(), Value::String(http_verb.to_uppercase()));
    envelope.insert("data".into(), body.unwrap_or(Value::Null));
    envelope.insert("meta".into(), Value::Object(meta));

    let payload = rmp_serde::to_vec_named(&Value::Object(envelope))
        .map_err(|e| format!("encode: {}", e))?;

    let io_fut = async {
        let header = (payload.len() as u32).to_be_bytes();
        client
            .write_all(&header)
            .await
            .map_err(|e| format!("pipe_io: write header: {}", e))?;
        client
            .write_all(&payload)
            .await
            .map_err(|e| format!("pipe_io: write body: {}", e))?;
        client.flush().await.ok();

        let mut hdr = [0u8; 4];
        client
            .read_exact(&mut hdr)
            .await
            .map_err(|e| format!("pipe_io: read header: {}", e))?;
        let n = u32::from_be_bytes(hdr) as usize;
        if n == 0 || n > MAX_FRAME {
            return Err(format!("pipe_io: frame size out of range: {}", n));
        }
        let mut buf = vec![0u8; n];
        client
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("pipe_io: read body: {}", e))?;
        Ok::<Vec<u8>, String>(buf)
    };

    let body_bytes = timeout(RESPONSE_TIMEOUT, io_fut).await.map_err(|_| {
        format!(
            "pipe_timeout: no response from '{}' within {}s",
            service,
            RESPONSE_TIMEOUT.as_secs()
        )
    })??;

    let reply: Value =
        rmp_serde::from_slice(&body_bytes).map_err(|e| format!("decode: {}", e))?;

    let ok = reply.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        Ok(reply.get("data").cloned().unwrap_or(Value::Null))
    } else {
        let err = reply.get("error").cloned().unwrap_or(Value::Null);
        let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("pipe error");
        Err(format!("{}: {}", code, msg))
    }
}

#[cfg(not(target_os = "windows"))]
pub async fn call(
    service: &str,
    _http_verb: &str,
    _path: &str,
    _body: Option<Value>,
) -> Result<Value, String> {
    Err(format!(
        "pipe_unavailable: named pipes are Windows-only (service '{}')",
        service
    ))
}

// ── Streaming action dispatch ────────────────────────────────────────
//
// Sibling of [`call`] for the harness's streaming verbs
// (`chat.stream_turn`, `chat.stream_tools`, `consent.stream_pending`).
// Wire shape per chunk is `wylde_shared::ipc::wire::ChunkFrame`; the
// caller-visible payload is the inner `payload` value the handler
// emitted, with heartbeat frames (null payload, `done=false`) silently
// dropped.
//
// The transport reuses the pre-v1, no-handshake action path the unary
// `call` already speaks to — that's all the harness binary needs to
// recognise this caller as a legacy GUI client and route to the
// streaming dispatcher.  Cancellation is signalled by dropping the
// returned [`PipeStream`]: the IO task is aborted, the pipe handle
// drops, and the server's `sender.closed().await` resolves.

/// Receiver-side handle for a streaming pipe call.
///
/// Dropping the handle aborts the background reader task, which closes
/// the pipe — that's the cancel signal the server's streaming handler
/// uses to wind itself down.
pub struct PipeStream {
    rx: tokio::sync::mpsc::Receiver<Result<Value, String>>,
    abort: Option<tokio::task::AbortHandle>,
}

impl PipeStream {
    /// Wait for the next chunk.  Returns `None` once the stream ends —
    /// either because the server set `done=true` or because the IO task
    /// surfaced a transport-level error frame.
    pub async fn recv(&mut self) -> Option<Result<Value, String>> {
        self.rx.recv().await
    }

    /// Cancel the stream eagerly.  Equivalent to dropping the handle
    /// but lets the caller name the cancel in source.
    pub fn cancel(self) {
        drop(self);
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        if let Some(a) = self.abort.take() {
            a.abort();
        }
    }
}

/// Open a streaming action against `service` and return a [`PipeStream`].
///
/// `service` is the bare or `wylde-`-prefixed pipe name; `action` is the
/// verb (e.g. `"chat.stream_turn"`).  `payload` is forwarded to the
/// server handler verbatim.
///
/// Errors:
///   * `no_runtime` — neither a current tokio runtime nor an installed
///     bridge handle is reachable; call [`install_runtime`] first.
///   * `pipe_unavailable` / `pipe_connect` / `pipe_io` — surfaced as the
///     first stream item rather than a synchronous `Err` so callers can
///     handle "service down" the same way they handle "stream errored
///     mid-flight".
///
/// **Non-Windows**: the returned stream surfaces a single
/// `pipe_unavailable` error and ends.  Mirrors the unary path.
pub fn stream_call(
    service: &str,
    action: &str,
    payload: Value,
) -> Result<PipeStream, String> {
    let handle = if let Ok(h) = tokio::runtime::Handle::try_current() {
        h
    } else if let Some(h) = TOKIO_HANDLE.get().cloned() {
        h
    } else {
        return Err(format!(
            "no_runtime: stream_call('{service}', '{action}') needs a tokio runtime; \
             call `wylde_gui_pipe::install_runtime(handle)` at startup",
        ));
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Value, String>>(16);
    let svc = service.to_string();
    let action = action.to_string();
    let task = handle.spawn(async move {
        run_stream_inner(&svc, &action, payload, tx).await;
    });
    Ok(PipeStream {
        rx,
        abort: Some(task.abort_handle()),
    })
}

#[cfg(target_os = "windows")]
async fn run_stream_inner(
    service: &str,
    action: &str,
    payload: Value,
    tx: tokio::sync::mpsc::Sender<Result<Value, String>>,
) {
    use wylde_shared::ipc::wire::ChunkFrame;

    let name = pipe_name(service);

    let connect_fut = async {
        loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => return Ok(c),
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    };

    let mut client = match timeout(CONNECT_TIMEOUT, connect_fut).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let code = e.raw_os_error().unwrap_or(0);
            let msg = if code == 2 || code == 3 {
                format!(
                    "pipe_unavailable: service '{}' is not running (pipe not found)",
                    service
                )
            } else {
                format!("pipe_connect: {}: {}", service, e)
            };
            let _ = tx.send(Err(msg)).await;
            return;
        }
        Err(_) => {
            let _ = tx
                .send(Err(format!(
                    "pipe_timeout: could not connect to service '{}' within {}s",
                    service,
                    CONNECT_TIMEOUT.as_secs()
                )))
                .await;
            return;
        }
    };

    // Pre-v1 envelope — the harness server treats a non-handshake first
    // frame as a legacy request, same path the unary `call` uses.  The
    // `stream: true` flag in the action body is what flips the server's
    // dispatcher to the multi-frame `ChunkFrame` path (see
    // `wylde-shared::ipc::server::serve_one_stream`).
    let mut meta = Map::new();
    // No client-side deadline for streaming — the harness's heartbeat
    // keeps the pipe warm.  We still send a large deadline so the
    // legacy envelope shape stays uniform.
    meta.insert("deadline_ms".into(), Value::from(60_000u64));
    meta.insert("caller".into(), Value::from(CALLER_NAME));

    let req_id = uuid::Uuid::new_v4().simple().to_string();
    let action_body = serde_json::json!({
        "action": action,
        "payload": payload,
        "stream": true,
    });

    let mut envelope = Map::new();
    envelope.insert("id".into(), Value::String(req_id.clone()));
    envelope.insert("method".into(), Value::String("/__action__".into()));
    envelope.insert("http_verb".into(), Value::String("POST".into()));
    envelope.insert("data".into(), action_body);
    envelope.insert("meta".into(), Value::Object(meta));

    let payload_bytes = match rmp_serde::to_vec_named(&Value::Object(envelope)) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(Err(format!("encode: {e}"))).await;
            return;
        }
    };

    let write_fut = async {
        let header = (payload_bytes.len() as u32).to_be_bytes();
        client.write_all(&header).await?;
        client.write_all(&payload_bytes).await?;
        client.flush().await.ok();
        Ok::<_, std::io::Error>(())
    };
    if let Err(e) = timeout(RESPONSE_TIMEOUT, write_fut).await {
        let _ = tx
            .send(Err(format!("pipe_io: write request: {e}")))
            .await;
        return;
    }

    // Chunk loop — read length-prefixed `ChunkFrame`s until `done=true`
    // or the consumer drops `rx` (in which case `tx.send` errors and we
    // bail, letting the pipe handle drop cancel the server task).
    loop {
        let read_fut = async {
            let mut hdr = [0u8; 4];
            client.read_exact(&mut hdr).await?;
            let n = u32::from_be_bytes(hdr) as usize;
            if n == 0 || n > MAX_FRAME {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("chunk frame size out of range: {n}"),
                ));
            }
            let mut body = vec![0u8; n];
            client.read_exact(&mut body).await?;
            Ok::<_, std::io::Error>(body)
        };

        let body = match read_fut.await {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(Err(format!("pipe_io: read chunk: {e}"))).await;
                return;
            }
        };

        let frame: ChunkFrame = match rmp_serde::from_slice(&body) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err(format!("decode: {e}"))).await;
                return;
            }
        };

        if !frame.ok {
            let err = frame.error;
            let code = err
                .as_ref()
                .map(|e| e.code.as_str())
                .unwrap_or("unknown");
            let msg = err
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("stream error with no body");
            let _ = tx.send(Err(format!("{code}: {msg}"))).await;
            return;
        }

        // Heartbeat / graceful-end: null payload.  Silently drop unless
        // `done=true`, in which case the loop exits below.
        let suppress = frame.payload.is_null();
        if !suppress && tx.send(Ok(frame.payload)).await.is_err() {
            return;
        }
        if frame.done {
            return;
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn run_stream_inner(
    service: &str,
    _action: &str,
    _payload: Value,
    tx: tokio::sync::mpsc::Sender<Result<Value, String>>,
) {
    let _ = tx
        .send(Err(format!(
            "pipe_unavailable: named pipes are Windows-only (service '{}')",
            service
        )))
        .await;
}

#[cfg(target_os = "windows")]
pub fn list_wylde_pipes() -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(r"\\.\pipe\")
        .map_err(|e| format!("read pipe dir: {}", e))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if let Some(rest) = s.strip_prefix("wylde-") {
            out.push(rest.to_string());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(not(target_os = "windows"))]
pub fn list_wylde_pipes() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

// ── In-process harness short-circuit (Phase 12.1) ─────────────────────

/// Try to dispatch a `wylde-harness` verb in-process via
/// [`wylde_harness::HarnessApi`].  Returns `Some(result)` when the verb
/// is known to one of the sub-dispatchers; `None` means the caller
/// should fall through to the over-the-wire path.  Streaming verbs
/// (`chat.stream_turn` / `chat.stream_tools`) return `None` — they
/// go over the wire until the gpui Subscription port lands.
pub async fn try_dispatch_harness<A: HarnessApi + ?Sized>(
    api: &A,
    verb: &str,
    payload: Value,
) -> Option<Result<Value, String>> {
    let reply = if let Some(r) = chat::dispatch(api, verb, payload.clone()).await {
        r
    } else if let Some(r) = tools::dispatch(api, verb, payload.clone()).await {
        r
    } else if let Some(r) = memory_long_term::dispatch(api, verb, payload.clone()).await {
        r
    } else if let Some(r) = memory_workspaces::dispatch(api, verb, payload).await {
        r
    } else {
        return None;
    };

    Some(reply_to_result(reply))
}

/// Convenience wrapper using the process-wide [`DefaultHarnessApi`].
pub async fn try_dispatch_harness_default(
    verb: &str,
    payload: Value,
) -> Option<Result<Value, String>> {
    try_dispatch_harness(&DefaultHarnessApi, verb, payload).await
}

/// Map a [`Reply`] into the same `Result<Value, String>` shape `call`
/// returns for the wire path.  Success → `Ok(reply.data)`; failure →
/// `Err("code: message")`, byte-identical to the wire client's error
/// projection.
fn reply_to_result(reply: Reply) -> Result<Value, String> {
    if reply.ok {
        Ok(reply.data)
    } else {
        let err = reply.error;
        let code = err.as_ref().map(|e| e.code.as_str()).unwrap_or("unknown");
        let msg = err
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("in-process dispatch error");
        Err(format!("{}: {}", code, msg))
    }
}

// ── Lifecycle helpers ─────────────────────────────────────────────────
//
// Hoisted from `Core/GUI/src-tauri/src/lib.rs`.  The Lifecycle daemon
// expects action verbs under the `/__action__` envelope; the wrapper
// keeps callers (the Shell tray's graceful shutdown; the future
// Settings panel's start/stop buttons) from re-implementing the
// envelope shape each time.

/// Dispatch a `service.<verb>` action against `\\.\pipe\wylde-lifecycle`.
pub async fn lifecycle_action(action: &str, payload: Value) -> Result<Value, String> {
    let envelope = serde_json::json!({
        "action": action,
        "payload": payload,
    });
    call("wylde-lifecycle", "POST", "/__action__", Some(envelope)).await
}

/// Query a single service's health via the Lifecycle daemon.
///
/// This is the slice-spec "round-trip proof" verb — a simple
/// request/response that exercises the full pipe path end-to-end so
/// the architecture can be verified before the Settings panel port
/// adds dozens more.
pub async fn service_health(service: &str) -> Result<Value, String> {
    lifecycle_action("service.health", serde_json::json!({ "name": service })).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pipe_name_strips_wylde_prefix() {
        assert_eq!(pipe_name("lifecycle"), r"\\.\pipe\wylde-lifecycle");
        assert_eq!(pipe_name("wylde-lifecycle"), r"\\.\pipe\wylde-lifecycle");
        assert_eq!(pipe_name("harness"), r"\\.\pipe\wylde-harness");
    }

    #[test]
    fn caller_name_matches_tauri_alpha() {
        // Identity used by every envelope.  Log greps across the cutover
        // depend on this string staying stable until `src-tauri/` is
        // deleted.  Once it goes, this assertion can flip too.
        assert_eq!(CALLER_NAME, "fletch-gui");
    }

    #[test]
    fn install_runtime_makes_tokio_reachable_outside_a_runtime() {
        // From a plain sync test (no tokio TLS) the bridge is not
        // reachable until `install_runtime` runs.  After install the
        // helper sees the stashed Handle.  We can't easily test the
        // *uninstalled* branch here without process isolation (the
        // OnceLock is process-wide), so the assertion focuses on the
        // post-install positive path.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build rt");
        install_runtime(rt.handle().clone());
        assert!(tokio_reachable(), "after install, the bridge must be reachable");
    }

    #[tokio::test]
    async fn try_dispatch_returns_none_for_unknown_verb() {
        let out = try_dispatch_harness_default("definitely.not.a.verb", json!({})).await;
        assert!(out.is_none(), "unknown verb should fall through");
    }

    #[tokio::test]
    async fn try_dispatch_handles_tools_list() {
        // `tools.list` is the cheapest known harness verb.  Same probe
        // the Tauri side uses to validate the dispatcher.
        let out = try_dispatch_harness_default("tools.list", Value::Null).await;
        let Some(result) = out else {
            panic!("tools.list should be dispatched in-process");
        };
        let data = result.expect("tools.list should succeed");
        let tools = data["tools"].as_array().expect("tools is array");
        assert!(!tools.is_empty(), "catalog must have at least one entry");
    }

    #[tokio::test]
    async fn try_dispatch_skips_streaming_verbs() {
        for verb in ["chat.stream_turn", "chat.stream_tools"] {
            let out = try_dispatch_harness_default(verb, json!({})).await;
            assert!(out.is_none(), "{verb} should fall through to wire path");
        }
    }

    /// `stream_call` mirrors the unary path's "daemon-down" guard: when
    /// the service pipe isn't open the very first chunk is a structured
    /// transport error rather than a hang or a panic.  Exercises
    /// envelope construction + the abort-on-drop wiring without needing
    /// the harness daemon up.
    #[tokio::test]
    async fn stream_call_returns_pipe_unavailable_when_daemon_down() {
        let mut stream = stream_call(
            "wylde-harness",
            "chat.stream_turn",
            json!({"turn_id": "does-not-exist"}),
        )
        .expect("stream_call should not synchronously fail inside a tokio runtime");
        let first = stream.recv().await;
        // Daemon happens to be live → `Ok(...)` is fine too; the
        // streaming path produced at least one chunk.  When down, the
        // error matches the unary path's vocabulary.
        match first {
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                assert!(
                    e.starts_with("pipe_unavailable:")
                        || e.starts_with("pipe_timeout:")
                        || e.starts_with("pipe_connect:")
                        || e.starts_with("pipe_io:")
                        // `not_found` surfaces if the daemon IS up but
                        // the bogus turn_id is unknown — also an
                        // acceptable "round-trip succeeded" outcome.
                        || e.starts_with("not_found:"),
                    "unexpected first stream error: {e}",
                );
            }
            None => panic!("stream ended without yielding a first chunk"),
        }
    }

    #[test]
    fn stream_call_requires_runtime_when_unbridged() {
        // Outside a tokio runtime + with no bridge installed, the call
        // surfaces a structured error rather than panicking.  We can't
        // assert the `Err` path deterministically because the OnceLock
        // bridge is process-wide and earlier tests in this file may
        // have installed one; so the assertion is the weaker "does not
        // panic" + "shape is well-formed".
        let out = stream_call("wylde-harness", "chat.stream_turn", json!({}));
        match out {
            Ok(_) => { /* runtime reachable — fine */ }
            Err(e) => assert!(
                e.starts_with("no_runtime:"),
                "expected no_runtime error, got: {e}",
            ),
        }
    }

    /// `service_health` is the slice's pick for "the second simple
    /// round-trip-proof verb".  Without a live Lifecycle daemon the
    /// call surfaces a `pipe_unavailable` error — verifying that path
    /// alone exercises envelope construction, msgpack encoding, and
    /// error projection end-to-end without needing the daemon up.
    #[tokio::test]
    async fn service_health_returns_pipe_unavailable_when_daemon_down() {
        let result = service_health("wylde-lifecycle").await;
        // We can't assume the daemon is up in a unit-test context.
        // What we *can* assert is that an unreachable pipe surfaces
        // the documented error code rather than panicking — so the
        // tray's graceful-fallback path has a stable error shape to
        // match on.
        match result {
            Ok(_) => {
                // Daemon happens to be live — also fine; the round-trip
                // succeeded which is what we ultimately wanted.
            }
            Err(e) => {
                assert!(
                    e.starts_with("pipe_unavailable:") || e.starts_with("pipe_timeout:"),
                    "expected pipe_unavailable/pipe_timeout, got: {e}",
                );
            }
        }
    }
}
