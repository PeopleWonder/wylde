//! Runtime control plane for the Lifecycle daemon.
//!
//! Rust port of `Core/Lifecycle/control.py`. The control plane is the
//! pipe action surface the GUI uses to drive start/stop/wake/list/
//! health on running services.
//!
//! Production actions (full registry + supervision behind them):
//!
//! * `service.list` — walks declarative manifests + runtime manifests
//!   under [`crate::registry`], probes pipe/port liveness, and shapes
//!   the result into the dashboard's expected envelope.
//! * `service.health` — pipe-probe the named service's liveness via
//!   [`wylde_shared::ipc::send_with_verb`]. Probes `/__ping__` (the
//!   Rust IPC server's built-in liveness method), not the Python-era
//!   `GET /health` route the Rust server never implemented — see the
//!   `LIVENESS_METHOD` doc for why probing `/health` reported every
//!   live Rust service as dead.
//! * `service.start` / `service.stop` — production spawn/stop for the
//!   six daemon-managed services. Routes the name to one of
//!   [`crate::state::services::start_<service>`] / `stop_<service>`.
//!   Services outside that set surface `not_registered` (start) /
//!   idempotent no-op success (stop) — see the divergence note below.
//! * `service.wake` — combines `service.start` + `service.health`.
//! * `service.shutdown_all` — drains every daemon-managed child
//!   (mirrors Python) and asks the daemon to exit after the reply has
//!   flushed.
//! * `lifecycle.shutdown_all` — synonym of `service.shutdown_all`.
//! * `lifecycle.status` / `lifecycle.list_services` /
//!   `lifecycle.start_service` — the no-spawn parity surface. They
//!   report (and, for `start_service`, drive) the would-have-spawned
//!   set without touching the launcher or registry, so they can be
//!   gated against the Python daemon's byte-identical handlers
//!   (`Core.Lifecycle.control`). See the no-spawn warning in
//!   [`crate::state`].
//!
//! ## Divergence from Python (surfaced, not papered over)
//!
//! Python's `service.start` / `service.stop` mutate `services.yaml`
//! and dispatch through the launcher, which can spawn *any* service
//! in the yaml. The Rust daemon has no launcher and never touches
//! `services.yaml`; it only spawns the six daemon-managed services.
//! For names outside that set:
//!
//! * `service.start` returns `not_registered` (Python would also
//!   return `not_registered` for a name absent from `services.yaml`,
//!   so the error envelope matches for any name the launcher couldn't
//!   spawn either way).
//! * `service.stop` returns idempotent no-op success
//!   `{name, status: "stopped", pid_killed: null}` — same envelope
//!   Python emits when the name is not tracked.
//!
//! The `tracked` field in `service.list` always reports `false`: Python
//! computes it as `name in _launcher.get_running()`, which is empty
//! for every daemon-managed service in either daemon (those services
//! are spawned through `daemon_state`, not the launcher).

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use wylde_shared::ipc::{
    register_action, send_with_verb, IpcError, Reply, ACTION_DISPATCH_PATH,
};

use crate::registry::{self, ServiceInfo};
use crate::state::services::{
    impl_for, start_device_gate, start_extension_bridge, start_gateway, start_harness,
    start_memgraph, start_ollama, start_treesitter, start_vpn, start_vram_broker, start_voice,
    stop_device_gate, stop_extension_bridge, stop_gateway, stop_harness, stop_memgraph,
    stop_ollama, stop_treesitter, stop_voice, stop_vpn, stop_vram_broker,
};
use crate::state::{
    is_service_alive, nospawn_enabled, nospawn_record, nospawn_snapshot, request_daemon_exit,
    service_name, service_pid, stop_all_daemon_managed, ShutdownSummary,
};

/// The eight daemon-managed services `lifecycle.start_service` accepts —
/// the subprocess set the no-spawn boot short-circuits. `memory_scheduler`
/// is excluded: it is an in-process subsystem, never a would-have-spawned
/// subprocess, on either daemon. `wylde-vpn` (Phase 2) is included even
/// though it's an `optional` tier service — daemon-managed lifecycle is
/// still authoritative for spawn/stop.
const DAEMON_MANAGED_SERVICES: [&str; 10] = [
    service_name::MEMGRAPH,
    service_name::VOICE,
    service_name::DEVICE_GATE,
    service_name::VRAM_BROKER,
    service_name::EXTENSION_BRIDGE,
    service_name::GATEWAY,
    service_name::OLLAMA,
    service_name::VPN,
    service_name::HARNESS,
    service_name::TREESITTER,
];

/// Heartbeat-age thresholds for the `service.list` status bucket.
/// Mirror the values in `Core/Lifecycle/control.py` and the GUI's
/// `deriveStatus` (`Core/GUI/src/lib/manifests.js`).
const ACTIVE_MAX_AGE_S: f64 = 90.0;
const STALE_MAX_AGE_S: f64 = 300.0;

/// Per-call timeout for the `service.health` pipe probe. Matches the
/// `timeout=5.0` in Python's `health_action`.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Method the health probe pings to decide liveness.
///
/// Python services registered a `GET /health` route, so the original
/// port probed `/health`. But the **Rust** IPC server
/// ([`wylde_shared::ipc::server`]) — which every Rust daemon (harness,
/// gateway, ollama, voice, vram-broker, extension-bridge, device-gate,
/// and the Lifecycle daemon itself) embeds — only implements three
/// built-in methods: `/__ping__`, `/__handshake__`, and `/__action__`.
/// A `GET /health` against any of them returns `no_handler`, which this
/// handler then maps to `service_unhealthy`. The net effect was that
/// `service.health` reported **every live Rust service as dead** even
/// while its pipe was bound and answering — the GUI's Dashboard showed
/// all-red dots and the Chat panel's required-service gate stubbed out
/// "wylde-harness is not running" against a perfectly healthy harness.
///
/// `/__ping__` is the Rust IPC server's native liveness primitive
/// (it replies `{pong: true, ver}` straight from the accept loop), so
/// it is the correct thing to probe for "is this service's pipe up and
/// dispatching?". The richer Python `/health` payloads were never
/// ported to the Rust server and no live consumer reads them — memgraph,
/// the one service with a data-bearing `/health`, no longer exposes a
/// pipe (it went direct-Bolt) and isn't probed through this path.
const LIVENESS_METHOD: &str = "/__ping__";

/// Transport-level `error.code` values emitted by
/// [`wylde_shared::ipc::send`] when the client itself could not reach
/// the target service (connect/handshake/IO/encode/decode/disabled).
///
/// `service.health` maps these to the `probe_failed` envelope — Python
/// distinguishes "couldn't talk to the service" (probe_failed) from
/// "service replied not-ok" (service_unhealthy) by catching exceptions
/// vs. inspecting the reply; in Rust both routes return a `Reply` with
/// `ok=false`, so we discriminate by code here instead.
const TRANSPORT_ERROR_CODES: &[&str] = &[
    "pipe_unavailable",
    "pipe_connect",
    "pipe_timeout",
    "pipe_io",
    "handshake_timeout",
    "handshake_io",
    "handshake_rejected",
    "encode",
    "decode",
    "ipc_disabled",
    "no_http_backend",
];

/// Bind each action to `wylde_shared::ipc` for pipe dispatch.
///
/// Called once at daemon boot. Re-registering is safe — the action
/// registry replaces the handler in place, same as the Python side.
pub fn register_with_ipc() {
    register_action("service.shutdown_all", |payload: Value| async move {
        shutdown_all_action(payload).await
    });
    register_action("service.list", |payload: Value| async move {
        service_list_action(payload).await
    });
    register_action("service.health", |payload: Value| async move {
        service_health_action(payload).await
    });
    register_action("service.start", |payload: Value| async move {
        service_start_action(payload).await
    });
    register_action("service.stop", |payload: Value| async move {
        service_stop_action(payload).await
    });
    register_action("service.wake", |payload: Value| async move {
        service_wake_action(payload).await
    });

    // No-spawn parity surface — byte-identical to the Python daemon's
    // `lifecycle.*` handlers (`Core.Lifecycle.control`).
    register_action("lifecycle.shutdown_all", |payload: Value| async move {
        shutdown_all_action(payload).await
    });
    register_action("lifecycle.status", |payload: Value| async move {
        lifecycle_status_action(payload).await
    });
    register_action("lifecycle.list_services", |payload: Value| async move {
        lifecycle_list_services_action(payload).await
    });
    register_action("lifecycle.start_service", |payload: Value| async move {
        lifecycle_start_service_action(payload).await
    });

    tracing::info!("control: registered 10 actions on wylde-lifecycle");
}

/// Payload returned by `service.shutdown_all`. Matches the Python
/// shape so the GUI's response handler doesn't need to branch on
/// which daemon answered.
#[derive(Debug, Clone, Serialize)]
struct ShutdownAllResponse {
    stopped: Vec<String>,
    count: usize,
    launcher_stopped: Vec<String>,
    daemon_managed_stopped: Vec<String>,
    daemon_managed_failed: Vec<Value>,
    daemon_will_exit: bool,
}

/// Stop every running service the daemon is tracking and ask the
/// daemon itself to exit shortly after.
///
/// The Python version also drives `launcher.shutdown_all()` to drain
/// non-core services launched from `services.yaml`. The Rust daemon
/// doesn't spawn those (launcher is Python-only) so `launcher_stopped`
/// is always empty here; the field is preserved for response-shape
/// parity with the Python daemon.
async fn shutdown_all_action(_payload: Value) -> Reply {
    let summary: ShutdownSummary = stop_all_daemon_managed().await;

    // Ask the daemon to flip its stop notify after a brief delay so
    // this reply flushes to the caller before the pipe server tears
    // down.
    let daemon_will_exit = request_daemon_exit(Duration::from_millis(500));

    let stopped = summary.stopped.clone();
    let daemon_managed_stopped = summary.stopped;
    let count = summary.count;
    let failed_json: Vec<Value> = summary
        .failed
        .into_iter()
        .map(|f| json!({"name": f.name, "error": f.error}))
        .collect();

    let response = ShutdownAllResponse {
        stopped,
        count,
        launcher_stopped: Vec::new(),
        daemon_managed_stopped,
        daemon_managed_failed: failed_json,
        daemon_will_exit,
    };

    match serde_json::to_value(&response) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(IpcError::new(
            "encode_failed",
            format!("shutdown_all response encode failed: {e}"),
        )),
    }
}

// ── Production service.* surface ───────────────────────────────────────
//
// Handlers below mirror Python's `Core.Lifecycle.control` byte-for-byte
// on the envelope. Helpers (`require_name`, `heartbeat_age`,
// `shape_service_list`, `dispatch_start`/`dispatch_stop`) are pulled out
// so they can be unit-tested without a live pipe.

/// Pull and validate `payload['name']`. Returns the trimmed name or a
/// `bad_request` [`IpcError`] (Python's `ControlError(code="bad_request")`
/// equivalent). Callers wrap with [`Reply::err`].
fn require_name(payload: &Value) -> Result<String, IpcError> {
    let obj = payload
        .as_object()
        .ok_or_else(|| IpcError::new("bad_request", "payload must be an object"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| IpcError::new("bad_request", "payload.name is required"))?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(IpcError::new("bad_request", "payload.name is required"));
    }
    Ok(trimmed.to_owned())
}

/// Seconds since `heartbeat`. `inf` if the string is missing or
/// unparseable. Delegates to the shared primitive so the daemon, the
/// dashboard classifier, and the pre-build guard agree on what counts
/// as stale.
fn heartbeat_age(heartbeat: Option<&str>) -> f64 {
    wylde_shared::manifest_status::heartbeat_age_secs(heartbeat)
}

/// Classify a registry entry into `active` / `stale` / `inactive`.
/// Pulled out so the `shape_service_list` test can drive it directly.
fn classify(info: &ServiceInfo) -> &'static str {
    if info.running {
        return "active";
    }
    let age = heartbeat_age(info.heartbeat.as_deref());
    if age < ACTIVE_MAX_AGE_S {
        "active"
    } else if age < STALE_MAX_AGE_S {
        "stale"
    } else {
        "inactive"
    }
}

/// Shape a `Vec<ServiceInfo>` into the GUI's `{services, counts}`
/// envelope. Pure function — extracted so unit tests can drive it with
/// hand-built `ServiceInfo`s without touching the filesystem.
fn shape_service_list(infos: Vec<ServiceInfo>) -> Value {
    let mut services: Vec<Value> = Vec::with_capacity(infos.len());
    let mut active = 0i64;
    let mut stale = 0i64;
    let mut inactive = 0i64;

    for info in infos {
        let bucket = classify(&info);
        match bucket {
            "active" => active += 1,
            "stale" => stale += 1,
            "inactive" => inactive += 1,
            _ => unreachable!("classify returns only the three buckets"),
        }
        let pipe = info.pipe.map(Value::String).unwrap_or(Value::Null);
        let port = info.port.map(Value::from).unwrap_or(Value::Null);
        let pid = info.pid.map(Value::from).unwrap_or(Value::Null);
        services.push(json!({
            "name": info.name,
            "version": info.version,
            "category": info.kind,
            "description": info.description,
            "port": port,
            "endpoint": Value::Null,
            "enabled": info.enabled,
            "pipe": pipe,
            "status": bucket,
            "running": info.running,
            "pid": pid,
            "started_at": info.started_at.unwrap_or_default(),
            "heartbeat": info.heartbeat.unwrap_or_default(),
            "contributes": info.contributes,
            // Rust daemon has no launcher; Python's `tracked` is
            // `info.name in _launcher.get_running()`, which is always
            // false for the services either daemon supervises through
            // `daemon_state`. See the divergence note in the module doc.
            "tracked": false,
            "source": info.source,
        }));
    }

    json!({
        "services": services,
        "counts": {
            "active": active,
            "stale": stale,
            "inactive": inactive,
        }
    })
}

/// Route a daemon-managed `name` to its `start_<service>` future.
async fn dispatch_start(name: &str) -> anyhow::Result<()> {
    match name {
        service_name::MEMGRAPH => start_memgraph().await,
        service_name::VOICE => start_voice().await,
        service_name::DEVICE_GATE => start_device_gate().await,
        service_name::VRAM_BROKER => start_vram_broker().await,
        service_name::EXTENSION_BRIDGE => start_extension_bridge().await,
        service_name::GATEWAY => start_gateway().await,
        service_name::OLLAMA => start_ollama().await,
        service_name::VPN => start_vpn().await,
        service_name::HARNESS => start_harness().await,
        service_name::TREESITTER => start_treesitter().await,
        _ => anyhow::bail!("not a daemon-managed service: {name}"),
    }
}

/// Route a daemon-managed `name` to its `stop_<service>` future.
async fn dispatch_stop(name: &str) -> anyhow::Result<()> {
    match name {
        service_name::MEMGRAPH => stop_memgraph().await,
        service_name::VOICE => stop_voice().await,
        service_name::DEVICE_GATE => stop_device_gate().await,
        service_name::VRAM_BROKER => stop_vram_broker().await,
        service_name::EXTENSION_BRIDGE => stop_extension_bridge().await,
        service_name::GATEWAY => stop_gateway().await,
        service_name::OLLAMA => stop_ollama().await,
        service_name::VPN => stop_vpn().await,
        service_name::HARNESS => stop_harness().await,
        service_name::TREESITTER => stop_treesitter().await,
        _ => anyhow::bail!("not a daemon-managed service: {name}"),
    }
}

/// Walk the service registry and return the dashboard's expected
/// envelope. Mirrors Python's `list_services_action` envelope shape.
async fn service_list_action(_payload: Value) -> Reply {
    let infos = registry::list_services();
    Reply::ok(shape_service_list(infos))
}

/// Probe `<name>`'s liveness over its pipe and shape the dashboard's
/// health envelope.
///
/// Probes [`LIVENESS_METHOD`] (`/__ping__`) — the Rust IPC server's
/// built-in liveness primitive — rather than the Python-era `GET
/// /health` route, which the Rust server never implemented (see the
/// [`LIVENESS_METHOD`] doc for the all-services-report-dead bug that
/// forced the switch). On success returns `{name, reply: <ping data>}`;
/// on transport failure `probe_failed`; on a service-level not-ok
/// `service_unhealthy`.
async fn service_health_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    // Memgraph special-case: the service deliberately retired its
    // named-pipe surface in the 2026-05-26 direct-Bolt cutover. The
    // wrapper (`Core/Memgraph/run.py`) now only supervises the Neo4j
    // JVM; the harness reads/writes the graph over Bolt directly, and
    // nothing binds `\\.\pipe\wylde-memgraph` anymore. Probing
    // `/__ping__` over that pipe therefore ALWAYS fails, false-reddening
    // the dashboard tile even though Neo4j is up. The honest liveness
    // signal for memgraph is "is Neo4j accepting Bolt connections?" —
    // probe the Bolt port instead of the dead pipe.
    if name == service_name::MEMGRAPH {
        return memgraph_health().await;
    }
    // Ollama special-case: a plain `/__ping__` only proves the wrapper's
    // pipe is up — it says nothing about whether the upstream Ollama
    // daemon on `127.0.0.1:11434` is actually reachable. That gap let the
    // Dashboard show a green ollama tile while inference was dead. Compose
    // the pipe liveness with the wrapper's own upstream probe so the
    // Dashboard can render a degraded/yellow tile.
    if name == service_name::OLLAMA {
        return ollama_health().await;
    }
    let reply = send_with_verb(&name, LIVENESS_METHOD, "GET", Value::Null, HEALTH_PROBE_TIMEOUT).await;
    if reply.ok {
        return Reply::ok(json!({
            "name": name,
            "reply": reply.data,
        }));
    }
    let err = reply
        .error
        .unwrap_or_else(|| IpcError::new("unknown", "unknown"));
    let (code, msg) = if TRANSPORT_ERROR_CODES.contains(&err.code.as_str()) {
        (
            "probe_failed",
            format!("health probe failed: {}", err.message),
        )
    } else {
        (
            "service_unhealthy",
            format!("{name} replied not-ok: {}", err.message),
        )
    };
    Reply::err(IpcError::new(code, msg))
}

/// Bolt-port liveness for `wylde-memgraph`. See [`service_health_action`]
/// for why memgraph is probed over Bolt rather than its (retired) pipe.
///
/// Honours `GRAPH_BOLT_PORT` (default 7687) to mirror the port
/// `Core/Memgraph/run.py` waits on and writes into the manifest. The
/// blocking TCP connect runs on a `spawn_blocking` thread so a slow
/// connect can't stall the daemon's async reactor; the success envelope
/// mirrors the `{pong: true, ...}` shape a pipe `/__ping__` would return
/// so the GUI's forgiving health projection treats it as healthy.
async fn memgraph_health() -> Reply {
    let port: i64 = std::env::var("GRAPH_BOLT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7687);
    let alive = tokio::task::spawn_blocking(move || registry::port_alive(Some(port)))
        .await
        .unwrap_or(false);
    if alive {
        Reply::ok(json!({
            "name": service_name::MEMGRAPH,
            "reply": { "pong": true, "transport": "bolt", "port": port },
        }))
    } else {
        Reply::err(IpcError::new(
            "probe_failed",
            format!("memgraph bolt probe failed: nothing accepting on 127.0.0.1:{port}"),
        ))
    }
}

/// Composed liveness for `wylde-ollama`. See [`service_health_action`]
/// for why ollama is special-cased.
///
/// Probing `/__ping__` over the wrapper pipe only answers "is the wrapper
/// up?". The honest liveness signal for the LLM layer is "is the upstream
/// Ollama daemon on `127.0.0.1:11434` reachable?", which only the wrapper
/// can answer. So instead of a bare ping we dispatch the wrapper's own
/// `ollama.health` action: reaching it over the pipe *is* the pipe-ping
/// (transport failure ⇒ `probe_failed`, a red tile), and its reply folds
/// in the upstream status (`ok` | `unreachable` | `timeout`) plus
/// `latency_ms`. The Dashboard reads `reply.upstream` / `reply.latency_ms`
/// to render the tri-state: green (upstream ok + fast), yellow (pipe up
/// but upstream down/slow), red (pipe down).
///
/// The envelope mirrors the generic path's `{name, reply}` shape so the
/// GUI's health projection treats it uniformly.
async fn ollama_health() -> Reply {
    let body = json!({ "action": "ollama.health", "payload": {} });
    let reply = send_with_verb(
        service_name::OLLAMA,
        ACTION_DISPATCH_PATH,
        "POST",
        body,
        HEALTH_PROBE_TIMEOUT,
    )
    .await;
    if reply.ok {
        // Wrapper answered over the pipe (pipe is up). `reply.data` carries
        // `{ok, pong, upstream, upstream_models?, latency_ms?}`.
        return Reply::ok(json!({
            "name": service_name::OLLAMA,
            "reply": reply.data,
        }));
    }
    // Wrapper didn't answer — mirror the generic path's discrimination:
    // a transport error means the pipe is down (`probe_failed`); anything
    // else is a service-level not-ok (`service_unhealthy`).
    let err = reply
        .error
        .unwrap_or_else(|| IpcError::new("unknown", "unknown"));
    let (code, msg) = if TRANSPORT_ERROR_CODES.contains(&err.code.as_str()) {
        (
            "probe_failed",
            format!("health probe failed: {}", err.message),
        )
    } else {
        (
            "service_unhealthy",
            format!("wylde-ollama replied not-ok: {}", err.message),
        )
    };
    Reply::err(IpcError::new(code, msg))
}

/// Production spawn for a daemon-managed service. Idempotent on
/// already-running. Names outside `DAEMON_MANAGED_SERVICES` get the
/// Python-compatible `not_registered` error envelope.
async fn service_start_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    if !DAEMON_MANAGED_SERVICES.contains(&name.as_str()) {
        return Reply::err(IpcError::new(
            "not_registered",
            format!("unknown service {name:?}"),
        ));
    }
    if is_service_alive(&name) {
        let pid = service_pid(&name).map(Value::from).unwrap_or(Value::Null);
        return Reply::ok(json!({
            "name": name,
            "status": "running",
            "pid": pid,
            "started": false,
        }));
    }
    match dispatch_start(&name).await {
        Ok(()) => {
            let pid = service_pid(&name).map(Value::from).unwrap_or(Value::Null);
            Reply::ok(json!({
                "name": name,
                "status": "running",
                "pid": pid,
                "started": true,
            }))
        }
        Err(e) => Reply::err(IpcError::new("spawn_failed", format!("spawn failed: {e:#}"))),
    }
}

/// Production stop for a daemon-managed service. Names outside
/// `DAEMON_MANAGED_SERVICES` return idempotent no-op success — Python
/// emits the same envelope when the name isn't tracked.
async fn service_stop_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    if !DAEMON_MANAGED_SERVICES.contains(&name.as_str()) {
        return Reply::ok(json!({
            "name": name,
            "status": "stopped",
            "pid_killed": Value::Null,
        }));
    }
    let pid_before = service_pid(&name);
    match dispatch_stop(&name).await {
        Ok(()) => {
            let pid_killed = pid_before.map(Value::from).unwrap_or(Value::Null);
            Reply::ok(json!({
                "name": name,
                "status": "stopped",
                "pid_killed": pid_killed,
            }))
        }
        Err(e) => Reply::err(IpcError::new("stop_failed", format!("stop failed: {e:#}"))),
    }
}

/// Ensure a daemon-managed service is running and responsive. If
/// already alive, probe `/health`; otherwise start it. Mirrors Python's
/// `wake_action` response shape: `{name, status, pid, woken, health?,
/// health_error?}`.
async fn service_wake_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    if !DAEMON_MANAGED_SERVICES.contains(&name.as_str()) {
        return Reply::err(IpcError::new(
            "not_registered",
            format!("unknown service {name:?}"),
        ));
    }
    if is_service_alive(&name) {
        let pid = service_pid(&name).map(Value::from).unwrap_or(Value::Null);
        let health = service_health_action(json!({ "name": name.clone() })).await;
        if health.ok {
            return Reply::ok(json!({
                "name": name,
                "status": "running",
                "pid": pid,
                "woken": false,
                "health": health.data,
            }));
        }
        let err = health
            .error
            .unwrap_or_else(|| IpcError::new("unknown", "unknown"));
        return Reply::ok(json!({
            "name": name,
            "status": "running",
            "pid": pid,
            "woken": false,
            "health_error": { "code": err.code, "message": err.message },
        }));
    }
    let started = service_start_action(json!({ "name": name })).await;
    if !started.ok {
        return started;
    }
    let mut data = started.data;
    if let Some(obj) = data.as_object_mut() {
        obj.insert("woken".to_string(), Value::Bool(true));
    }
    Reply::ok(data)
}

// ── No-spawn parity surface ────────────────────────────────────────────
//
// `lifecycle.status` / `lifecycle.list_services` / `lifecycle.start_service`
// report (and, for `start_service`, drive) the would-have-spawned set. Each
// returns the byte-equivalent envelope of the matching Python handler in
// `Core.Lifecycle.control`, so the parity suite can gate them. See the
// no-spawn warning in `crate::state`.

/// Report the daemon's no-spawn status. `nospawn` is the mode flag;
/// `would_have_spawned` is the sorted list of services the daemon
/// short-circuited instead of forking (empty in a production daemon).
async fn lifecycle_status_action(_payload: Value) -> Reply {
    let snapshot = nospawn_snapshot();
    Reply::ok(json!({
        "nospawn": nospawn_enabled(),
        "service_count": snapshot.len(),
        "would_have_spawned": snapshot,
    }))
}

/// Map each would-have-spawned daemon-managed service to its state. The
/// map is empty in a production daemon (no-spawn off).
async fn lifecycle_list_services_action(_payload: Value) -> Reply {
    let snapshot = nospawn_snapshot();
    let mut services = serde_json::Map::new();
    for name in &snapshot {
        services.insert(
            name.clone(),
            Value::String("would-have-spawned".to_string()),
        );
    }
    Reply::ok(json!({
        "services": Value::Object(services),
        "count": snapshot.len(),
    }))
}

/// Run the no-spawn would-have-spawned short-circuit for `payload.name`.
///
/// Returns the synthetic success envelope a real spawn mirrors. No-spawn
/// only: rejects with `nospawn_required` when no-spawn mode is off, so it
/// can never trigger a real child process.
async fn lifecycle_start_service_action(payload: Value) -> Reply {
    if !nospawn_enabled() {
        return Reply::err(IpcError::new(
            "nospawn_required",
            "lifecycle.start_service is a no-spawn-only parity action",
        ));
    }
    let name = match payload.get("name").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            return Reply::err(IpcError::new("bad_request", "payload.name is required"));
        }
    };
    if !DAEMON_MANAGED_SERVICES.contains(&name.as_str()) {
        return Reply::err(IpcError::new(
            "unknown_service",
            format!("unknown daemon-managed service {name:?}"),
        ));
    }
    // Record the would-have-spawned entry — the no-spawn short-circuit a
    // real `services::start_<service>` performs. Idempotent: the boot
    // sequence already recorded the full set.
    nospawn_record(&name, impl_for(&name).as_str());
    Reply::ok(json!({
        "name": name,
        "status": "would-have-spawned",
        "would_have_spawned": true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_actions, unregister_action};

    // The action registry is a process-global. Without a guard,
    // parallel tests race each other's register/cleanup pairs and
    // some lookups land between a sibling's cleanup and re-register.
    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    /// Every action `register_with_ipc` binds — kept in one place so the
    /// cleanup and the registration assertion can't drift apart.
    const ALL_ACTIONS: [&str; 10] = [
        "service.shutdown_all",
        "lifecycle.shutdown_all",
        "lifecycle.status",
        "lifecycle.list_services",
        "lifecycle.start_service",
        "service.start",
        "service.stop",
        "service.wake",
        "service.list",
        "service.health",
    ];

    fn cleanup() {
        for n in ALL_ACTIONS {
            unregister_action(n);
        }
    }

    /// Drop every no-spawn record and pin the flag off — leaves the
    /// process-global state clean for the next state-mutating test.
    fn clear_nospawn() {
        for n in crate::state::nospawn_snapshot() {
            crate::state::nospawn_take(&n);
        }
        crate::state::set_nospawn(false);
    }

    #[tokio::test]
    async fn registers_all_actions() {
        let _g = registry_guard().await;
        cleanup();
        register_with_ipc();
        let actions = list_actions();
        for n in ALL_ACTIONS {
            assert!(actions.contains(&n.to_string()), "missing action {n}");
        }
        cleanup();
    }

    // ── service.* production-handler tests ──────────────────────────

    fn fresh_info(name: &str, kind: &str) -> ServiceInfo {
        ServiceInfo {
            name: name.to_owned(),
            kind: kind.to_owned(),
            source: "manifest".to_owned(),
            contributes: json!({}),
            ..ServiceInfo::default()
        }
    }

    #[test]
    fn heartbeat_age_handles_missing_and_malformed() {
        assert!(heartbeat_age(None).is_infinite());
        assert!(heartbeat_age(Some("")).is_infinite());
        assert!(heartbeat_age(Some("not a date")).is_infinite());
    }

    #[test]
    fn heartbeat_age_parses_z_suffix() {
        // A "recent enough to be active" timestamp (well within the past).
        // Use a fixed UTC string and assert the result is finite + positive.
        let ts = chrono::Utc::now() - chrono::Duration::seconds(30);
        let s = ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let age = heartbeat_age(Some(&s));
        assert!(age.is_finite() && (20.0..=60.0).contains(&age), "age={age}");
    }

    #[test]
    fn shape_service_list_empty_has_zero_counts() {
        let v = shape_service_list(Vec::new());
        assert_eq!(v["services"], json!([]));
        assert_eq!(v["counts"]["active"], 0);
        assert_eq!(v["counts"]["stale"], 0);
        assert_eq!(v["counts"]["inactive"], 0);
    }

    #[test]
    fn shape_service_list_running_is_active() {
        let mut info = fresh_info("wylde-x", "standard");
        info.running = true;
        let v = shape_service_list(vec![info]);
        assert_eq!(v["counts"]["active"], 1);
        assert_eq!(v["services"][0]["status"], "active");
        assert_eq!(v["services"][0]["running"], true);
        // Verify the Python-parity envelope keys are all present.
        for key in [
            "name",
            "version",
            "category",
            "description",
            "port",
            "endpoint",
            "enabled",
            "pipe",
            "status",
            "running",
            "pid",
            "started_at",
            "heartbeat",
            "contributes",
            "tracked",
            "source",
        ] {
            assert!(
                v["services"][0].get(key).is_some(),
                "missing key {key} in shaped service",
            );
        }
        assert_eq!(v["services"][0]["tracked"], false);
        assert_eq!(v["services"][0]["endpoint"], Value::Null);
    }

    #[test]
    fn shape_service_list_buckets_by_heartbeat_age() {
        let now = chrono::Utc::now();
        let fresh = (now - chrono::Duration::seconds(30)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let aged = (now - chrono::Duration::seconds(150)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let dead = (now - chrono::Duration::seconds(600)).format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let mut a = fresh_info("wylde-a", "standard");
        a.heartbeat = Some(fresh);
        let mut s = fresh_info("wylde-s", "standard");
        s.heartbeat = Some(aged);
        let mut i = fresh_info("wylde-i", "standard");
        i.heartbeat = Some(dead);

        let v = shape_service_list(vec![a, s, i]);
        assert_eq!(v["counts"]["active"], 1);
        assert_eq!(v["counts"]["stale"], 1);
        assert_eq!(v["counts"]["inactive"], 1);
    }

    #[test]
    fn require_name_rejects_non_object_payload() {
        let err = require_name(&Value::Null).expect_err("expected bad_request");
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn require_name_rejects_missing_name() {
        let err = require_name(&json!({})).expect_err("expected bad_request");
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn require_name_rejects_blank_name() {
        let err = require_name(&json!({"name": "   "})).expect_err("expected bad_request");
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn require_name_trims_whitespace() {
        let ok = require_name(&json!({"name": "  wylde-voice  "})).expect("ok");
        assert_eq!(ok, "wylde-voice");
    }

    #[tokio::test]
    async fn service_start_unknown_returns_not_registered() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        let reply = dispatch_action(json!({
            "action": "service.start",
            "payload": {"name": "wylde-parity-bogus"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_registered");

        cleanup();
    }

    #[tokio::test]
    async fn service_stop_unknown_is_noop_success() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        let reply = dispatch_action(json!({
            "action": "service.stop",
            "payload": {"name": "wylde-parity-bogus"},
        }))
        .await;
        assert!(reply.ok, "expected ok, got {reply:?}");
        assert_eq!(reply.data["name"], "wylde-parity-bogus");
        assert_eq!(reply.data["status"], "stopped");
        assert_eq!(reply.data["pid_killed"], Value::Null);

        cleanup();
    }

    #[tokio::test]
    async fn service_wake_unknown_returns_not_registered() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        let reply = dispatch_action(json!({
            "action": "service.wake",
            "payload": {"name": "wylde-parity-bogus"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_registered");

        cleanup();
    }

    #[tokio::test]
    async fn service_health_unreachable_returns_probe_failed() {
        let _g = registry_guard().await;
        cleanup();
        register_with_ipc();

        // Guaranteed-missing pipe name → send_with_verb returns
        // pipe_connect/pipe_unavailable → mapped to probe_failed.
        let svc = format!("wylde-parity-missing-{}", uuid_like_suffix());
        let reply = dispatch_action(json!({
            "action": "service.health",
            "payload": {"name": svc},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "probe_failed");

        cleanup();
    }

    #[tokio::test]
    async fn service_health_ollama_routes_through_composed_probe() {
        let _g = registry_guard().await;
        cleanup();
        register_with_ipc();

        // `wylde-ollama` is special-cased to compose pipe liveness with
        // the wrapper's upstream probe. The exact outcome depends on
        // whether the wrapper pipe is up in this env, so assert the
        // *shape* of each branch rather than pinning one:
        //   * pipe up   → ok envelope `{name, reply: {upstream, ...}}`
        //   * pipe down → `probe_failed` (red tile)
        let reply = dispatch_action(json!({
            "action": "service.health",
            "payload": {"name": "wylde-ollama"},
        }))
        .await;
        if reply.ok {
            assert_eq!(reply.data["name"], "wylde-ollama");
            assert!(
                reply.data["reply"]["upstream"].is_string(),
                "composed ollama health must fold in the upstream status, got {:?}",
                reply.data,
            );
        } else {
            assert_eq!(reply.error.unwrap().code, "probe_failed");
        }

        cleanup();
    }

    #[test]
    fn liveness_method_is_the_rust_ipc_ping_primitive() {
        // Regression for the "GUI shows every live service as dead" bug
        // (2026-05-31): the probe must hit the Rust IPC server's built-in
        // `/__ping__` liveness method, NOT the Python-era `/health` route
        // that the Rust server returns `no_handler` for. Probing `/health`
        // made `service.health` report `service_unhealthy` for every Rust
        // daemon whose pipe was bound and answering, all-red'ing the
        // Dashboard and stubbing out the Chat panel.
        assert_eq!(LIVENESS_METHOD, "/__ping__");
        assert_ne!(LIVENESS_METHOD, "/health");
    }

    #[tokio::test]
    async fn service_action_missing_name_is_bad_request() {
        let _g = registry_guard().await;
        cleanup();
        register_with_ipc();

        for action in ["service.start", "service.stop", "service.wake", "service.health"] {
            let reply = dispatch_action(json!({
                "action": action,
                "payload": {},
            }))
            .await;
            assert!(!reply.ok, "{action}: expected bad_request, got {reply:?}");
            assert_eq!(reply.error.unwrap().code, "bad_request", "{action}");
        }

        cleanup();
    }

    #[tokio::test]
    async fn service_list_returns_shaped_envelope() {
        let _g = registry_guard().await;
        cleanup();
        register_with_ipc();

        let reply = dispatch_action(json!({
            "action": "service.list",
            "payload": null,
        }))
        .await;
        // We don't pin a specific service set (depends on WYLDE_ROOT in
        // the test env), just the envelope shape.
        assert!(reply.ok, "expected ok, got {reply:?}");
        assert!(reply.data["services"].is_array());
        assert!(reply.data["counts"].is_object());
        assert!(reply.data["counts"]["active"].is_i64());
        assert!(reply.data["counts"]["stale"].is_i64());
        assert!(reply.data["counts"]["inactive"].is_i64());

        cleanup();
    }

    fn uuid_like_suffix() -> String {
        // Cheap unique-ish suffix — avoids pulling in `uuid` for tests.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{nanos:x}")
    }

    #[tokio::test]
    async fn shutdown_all_returns_structured_summary() {
        let _g = registry_guard().await;
        // Serialise against state-mutating tests: `stop_all_daemon_managed`
        // reads the process-global no-spawn flag, which the `state` module's
        // no-spawn test toggles. Lock order is always registry-then-state.
        let _sg = crate::state::tests::state_guard().await;
        // This test exercises the real (spawning) path; pin the flag off so a
        // prior test's leftover can't flip us into the no-spawn branch.
        crate::state::set_nospawn(false);
        cleanup();
        register_with_ipc();
        // No services are spawned in this unit test; the action
        // should still return ok with empty lists. The
        // `daemon_will_exit` flag depends on whether a stop event is
        // registered — left unasserted here because other tests in
        // this crate set/clear it; we only check field presence +
        // type.
        let reply = dispatch_action(json!({
            "action": "service.shutdown_all",
            "payload": null,
        }))
        .await;
        assert!(reply.ok, "expected ok, got {reply:?}");
        assert_eq!(reply.data["count"], 0);
        assert!(reply.data["launcher_stopped"].is_array());
        assert!(reply.data["daemon_managed_stopped"].is_array());
        assert!(reply.data["daemon_will_exit"].is_boolean());
        cleanup();
    }

    #[tokio::test]
    async fn lifecycle_status_reports_would_have_spawned() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        crate::state::set_nospawn(true);
        crate::state::nospawn_record("wylde-voice", "python");
        crate::state::nospawn_record("wylde-gateway", "rust");

        let reply = dispatch_action(json!({
            "action": "lifecycle.status",
            "payload": null,
        }))
        .await;
        assert!(reply.ok, "expected ok, got {reply:?}");
        assert_eq!(reply.data["nospawn"], true);
        assert_eq!(reply.data["service_count"], 2);
        assert_eq!(
            reply.data["would_have_spawned"],
            json!(["wylde-gateway", "wylde-voice"]),
        );

        clear_nospawn();
        cleanup();
    }

    #[tokio::test]
    async fn lifecycle_list_services_maps_each_to_state() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        crate::state::set_nospawn(true);
        crate::state::nospawn_record("wylde-memgraph", "python");

        let reply = dispatch_action(json!({
            "action": "lifecycle.list_services",
            "payload": null,
        }))
        .await;
        assert!(reply.ok, "expected ok, got {reply:?}");
        assert_eq!(reply.data["count"], 1);
        assert_eq!(
            reply.data["services"]["wylde-memgraph"],
            "would-have-spawned",
        );

        clear_nospawn();
        cleanup();
    }

    #[tokio::test]
    async fn lifecycle_start_service_records_would_have_spawned() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        crate::state::set_nospawn(true);
        let reply = dispatch_action(json!({
            "action": "lifecycle.start_service",
            "payload": {"name": "wylde-voice"},
        }))
        .await;
        assert!(reply.ok, "expected ok, got {reply:?}");
        assert_eq!(reply.data["name"], "wylde-voice");
        assert_eq!(reply.data["status"], "would-have-spawned");
        assert_eq!(reply.data["would_have_spawned"], true);
        // The short-circuit actually recorded the entry.
        assert!(crate::state::nospawn_snapshot().contains(&"wylde-voice".to_string()));

        clear_nospawn();
        cleanup();
    }

    #[tokio::test]
    async fn lifecycle_start_service_requires_nospawn() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn(); // no-spawn pinned OFF
        cleanup();
        register_with_ipc();

        let reply = dispatch_action(json!({
            "action": "lifecycle.start_service",
            "payload": {"name": "wylde-voice"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "nospawn_required");

        cleanup();
    }

    #[tokio::test]
    async fn lifecycle_start_service_rejects_unknown() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        crate::state::set_nospawn(true);
        let reply = dispatch_action(json!({
            "action": "lifecycle.start_service",
            "payload": {"name": "wylde-bogus"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "unknown_service");

        clear_nospawn();
        cleanup();
    }
}
