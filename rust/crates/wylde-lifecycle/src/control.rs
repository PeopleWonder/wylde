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

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use serde_json::{json, Value};
use wylde_shared::ipc::{register_action, send_with_verb, IpcError, Reply, ACTION_DISPATCH_PATH};

use crate::registry::{self, ServiceInfo};
use crate::state::services::{
    impl_for, rust_binary_path, start_device_gate, start_discovered, start_extension_bridge,
    start_gateway, start_harness, start_memgraph, start_n8n, start_ollama, start_treesitter,
    start_voice, start_vpn, start_vram_broker, start_workspaces, stop_device_gate, stop_discovered,
    stop_extension_bridge, stop_gateway, stop_harness, stop_memgraph, stop_n8n, stop_ollama,
    stop_treesitter, stop_voice, stop_vpn, stop_vram_broker, stop_workspaces,
};
use crate::state::{
    is_service_alive, nospawn_enabled, nospawn_record, nospawn_snapshot, request_daemon_exit,
    service_name, service_pid, stop_all_daemon_managed, ShutdownSummary,
};

/// The **core, in-tree** daemon-managed services — the bespoke,
/// dependency-ordered set the daemon compiles in and supervises with
/// hand-written `start_<service>` / `stop_<service>` fns. `memory_scheduler`
/// is excluded: it is an in-process subsystem, never a would-have-spawned
/// subprocess, on either daemon. `wylde-vpn` (Phase 2) is included even
/// though it's an `optional` tier service — daemon-managed lifecycle is
/// still authoritative for spawn/stop. `wylde-workspaces` (Thought Bubble
/// System Phase 0, Slice A) consumes ollama/treesitter/memgraph and is
/// spawned after them. `wylde-n8n` (taxonomy reorg TX S3) is the optional
/// pipe surface over the external, user-managed n8n daemon.
///
/// This is **no longer the authority** on what `service.start` / `.stop` /
/// `.wake` accept — that gate is now discovery-driven ([`is_manageable`]),
/// so an out-of-tree sibling dropped into the `Services/*` bucket is
/// accepted and supervised without editing any hardcoded list. This const
/// remains only as the core subset: the names with a `dispatch_start`
/// match arm and the no-spawn parity surface
/// (`lifecycle.start_service`, byte-identical to the Python daemon).
const CORE_SERVICES: [&str; 12] = [
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
    service_name::WORKSPACES,
    service_name::N8N,
];

/// Is `name` a service this daemon can start/stop/wake? True for a **core**
/// in-tree service ([`CORE_SERVICES`]) OR a **discovered out-of-tree
/// sibling** under the `Services/*` bucket. This replaces the old fixed
/// accept-list array: the manageable set is now discovery-driven, so
/// nothing is hardcoded for buckets — a sibling appears the moment it is
/// dropped in, and disappears (cleanly, `not_registered`) when removed.
fn is_manageable(name: &str) -> bool {
    CORE_SERVICES.contains(&name) || is_discovered_sibling(name)
}

/// Does the live `Services/*` discovery currently include `name`?
fn is_discovered_sibling(name: &str) -> bool {
    registry::discovered_bucket_services()
        .iter()
        .any(|d| d.name == name)
}

/// Heartbeat-age thresholds for the `service.list` status bucket.
/// Mirror the values in `Core/Lifecycle/control.py` and the GUI's
/// `deriveStatus` (`Core/GUI/src/lib/manifests.js`).
const ACTIVE_MAX_AGE_S: f64 = 90.0;
const STALE_MAX_AGE_S: f64 = 300.0;

/// Grace window for the F1 staleness guard. A service is only flagged as
/// running a stale binary when its on-disk binary is clearly newer than the
/// live process (by more than this many seconds), so clock jitter or a
/// rebuild-then-start within the same window never false-positives.
const STALE_BINARY_GRACE_S: f64 = 30.0;

/// Per-call timeout for the `service.health` pipe probe. Matches the
/// `timeout=5.0` in Python's `health_action`.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-call timeout for dispatching `ollama.health` to the wrapper while
/// `service.start wylde-ollama` ensures the upstream daemon. Short — it is
/// only asking the (already-up) wrapper whether 127.0.0.1:11434 answers.
const OLLAMA_UPSTREAM_PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// How long `service.start wylde-ollama` waits for a freshly-spawned
/// `ollama serve` to start answering before giving up. Stays under the
/// GUI pipe's 30 s response budget so the button's click resolves.
const OLLAMA_UPSTREAM_START_DEADLINE: Duration = Duration::from_secs(15);

/// Poll cadence while waiting for the freshly-spawned Ollama daemon.
const OLLAMA_UPSTREAM_START_POLL: Duration = Duration::from_millis(400);

/// Env flag that gates the DEV-ONLY `dev.restart_service` action.
///
/// Set (to a truthy value) ONLY by `tools/dev/wylde-dev.ps1` when it
/// boots the daemon for the full-stack hot-reload loop. A production /
/// release daemon never sets it, so [`register_with_ipc`] never binds
/// the action and the handler — were it ever reached — refuses with
/// `not_dev_mode`. There is no release code path that flips this on; it
/// exists purely so the backend file-watcher can bounce a single
/// just-rebuilt service without restarting the whole stack. See
/// `outputs/dev-fullstack-hotreload-report.md`.
const DEV_HOTRELOAD_ENV: &str = "WYLDE_DEV_HOTRELOAD";

/// Truthy-parse the dev hot-reload gate. Mirrors the daemon's no-spawn
/// truthiness set so `1/true/yes/on` all enable it.
fn dev_hotreload_enabled() -> bool {
    matches!(
        std::env::var(DEV_HOTRELOAD_ENV)
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

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

    // Self-updater preferences (Phase 12.5). The Settings → Updates
    // section dispatches these against the lifecycle daemon; without them
    // every toggle landed on `no_action` and the read fell back to
    // hard-coded defaults (see `updater_prefs`).
    register_action("updater.get_prefs", |payload: Value| async move {
        updater_get_prefs_action(payload).await
    });
    register_action("updater.set_prefs", |payload: Value| async move {
        updater_set_prefs_action(payload).await
    });

    // Per-service user-data path store (out-of-tree foundation, plan §3).
    // The first-open picker / Settings dispatch these to learn + persist
    // where a data-owning service's library lives; the daemon injects the
    // resolved path as WYLDE_<SVC>_DATA_DIR at spawn.
    register_action("paths.get", |payload: Value| async move {
        paths_get_action(payload).await
    });
    register_action("paths.set", |payload: Value| async move {
        paths_set_action(payload).await
    });

    tracing::info!("control: registered 14 actions on wylde-lifecycle");

    // DEV-ONLY: the full-stack hot-reload restart hook. Bound ONLY when
    // `WYLDE_DEV_HOTRELOAD` is truthy — set exclusively by
    // `tools/dev/wylde-dev.ps1`. A normal/release daemon never sets the
    // flag, so this action simply does not exist there (no release
    // behaviour change whatsoever). See `dev_restart_service_action`.
    if dev_hotreload_enabled() {
        register_action("dev.restart_service", |payload: Value| async move {
            dev_restart_service_action(payload).await
        });
        tracing::warn!(
            "control: DEV hot-reload active ({DEV_HOTRELOAD_ENV}) — bound dev.restart_service; \
             this must never appear in a release daemon"
        );
    }
}

/// DEV-ONLY per-service restart hook for the full-stack hot-reload loop.
///
/// Gated behind [`DEV_HOTRELOAD_ENV`] at *both* registration time (see
/// [`register_with_ipc`]) and here (defence-in-depth: a stale handler
/// can never act in a daemon where the flag was cleared). Graceful,
/// single-service bounce that leaves the GUI and every other service
/// untouched:
///
///   1. `dispatch_stop(name)` — the same graceful CTRL_BREAK + wait +
///      force-kill teardown a production `service.stop` uses, releasing
///      the Windows sharing lock on the staged `.exe`.
///   2. *swap* (optional) — copy the freshly-built `payload.binary` over
///      the service's staged binary (the `WYLDE_<NAME>_BIN` override the
///      dev daemon spawns from). The copy happens only AFTER the stop, so
///      the dest is unlocked. On a copy failure the previous binary is
///      respawned so the service is never left dark.
///   3. `dispatch_start(name)` — respawns from the (possibly swapped)
///      staged path; the new child rebinds its pipe and consumers
///      re-handshake lazily on their next request.
///
/// The watcher only calls this on a SUCCESSFUL build, so a build failure
/// never reaches here — the old service keeps running and the watcher
/// surfaces the compiler error itself.
async fn dev_restart_service_action(payload: Value) -> Reply {
    if !dev_hotreload_enabled() {
        return Reply::err(IpcError::new(
            "not_dev_mode",
            format!("dev.restart_service is gated behind {DEV_HOTRELOAD_ENV}"),
        ));
    }
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    if !is_manageable(&name) {
        return Reply::err(IpcError::new(
            "not_registered",
            format!("unknown service {name:?}"),
        ));
    }

    // 1. Graceful stop — releases the staged-exe sharing lock.
    if let Err(e) = dispatch_stop(&name).await {
        return Reply::err(IpcError::new("stop_failed", format!("stop failed: {e:#}")));
    }

    // 2. Optional binary swap (the service is stopped, so its staged
    //    `.exe` is now writable). On failure, respawn the OLD bytes so we
    //    never leave the service dark, and surface the swap error.
    let mut swapped = false;
    if let Some(src) = payload.get("binary").and_then(Value::as_str) {
        match swap_staged_binary(&name, src) {
            Ok(()) => swapped = true,
            Err(e) => {
                let _ = dispatch_start(&name).await; // wylde-check: discard-result-ok
                return Reply::err(IpcError::new(
                    "swap_failed",
                    format!("binary swap failed: {e:#}; respawned previous binary"),
                ));
            }
        }
    }

    // 3. Respawn from the (possibly swapped) staged path.
    if let Err(e) = dispatch_start(&name).await {
        return Reply::err(IpcError::new(
            "spawn_failed",
            format!("respawn failed: {e:#}"),
        ));
    }

    let pid = service_pid(&name).map(Value::from).unwrap_or(Value::Null);
    Reply::ok(json!({
        "name": name,
        "status": "restarted",
        "pid": pid,
        "swapped": swapped,
    }))
}

/// Resolve the staged binary path the dev daemon spawns `name` from —
/// the `WYLDE_<NAME>_BIN` override (`wylde-workspaces` →
/// `WYLDE_WYLDE_WORKSPACES_BIN`). `None` when the override is unset (the
/// daemon isn't in the dev BIN-override configuration).
fn staged_binary_target(name: &str) -> Option<PathBuf> {
    let var = format!("WYLDE_{}_BIN", name.to_uppercase().replace('-', "_"));
    std::env::var_os(var).map(PathBuf::from)
}

/// Copy a freshly-built `src` binary over `name`'s staged binary. The
/// caller MUST have stopped the service first (else the dest `.exe` is
/// locked on Windows). Pulled out so it can be unit-tested without a
/// live daemon.
fn swap_staged_binary(name: &str, src: &str) -> anyhow::Result<()> {
    let dest = staged_binary_target(name).ok_or_else(|| {
        anyhow::anyhow!(
            "no stage target: WYLDE_{}_BIN is unset",
            name.to_uppercase().replace('-', "_")
        )
    })?;
    let src_path = Path::new(src);
    if !src_path.exists() {
        anyhow::bail!("source binary does not exist: {src}");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create stage dir {}", parent.display()))?;
    }
    std::fs::copy(src_path, &dest)
        .with_context(|| format!("copy {} -> {}", src, dest.display()))?;
    Ok(())
}

/// `updater.get_prefs` — return the persisted updater prefs (defaults
/// when the file is missing/unreadable).
async fn updater_get_prefs_action(_payload: Value) -> Reply {
    Reply::ok(crate::updater_prefs::load().to_value())
}

/// `updater.set_prefs` — merge the partial patch into the on-disk prefs
/// and return the merged shape. The whole payload object *is* the patch
/// (the GUI sends e.g. `{"channel":"beta"}` or `{"last_checked":N}`).
async fn updater_set_prefs_action(payload: Value) -> Reply {
    let mut prefs = crate::updater_prefs::load();
    prefs.apply_patch(&payload);
    if let Err(e) = crate::updater_prefs::save(&prefs) {
        return Reply::err_msg("io_error", format!("persist updater prefs: {e}"));
    }
    Reply::ok(prefs.to_value())
}

/// `paths.get` — resolve a service's user-data dir. Payload `{name}`.
/// Returns `{name, data_dir, source}` where `source` is `"override"` (a
/// persisted `paths.set`) or `"default"` (the computed
/// `WyldeData/<svc>/` sibling). Never errors for an unknown service — a
/// name with no override simply resolves to its default.
async fn paths_get_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    let store = crate::paths::load();
    let (data_dir, source) = match store.get(&name) {
        Some(over) => (PathBuf::from(over), "override"),
        None => (crate::paths::default_data_dir(&name), "default"),
    };
    Reply::ok(json!({
        "name": name,
        "data_dir": data_dir.to_string_lossy(),
        "source": source,
    }))
}

/// `paths.set` — persist a service's user-data dir. Payload
/// `{name, data_dir}`. Writes the override (atomic) and returns the
/// resolved shape (`source: "override"`). An empty/missing `data_dir`
/// clears the override (the service reverts to its default on next read).
async fn paths_set_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    let data_dir = payload
        .get("data_dir")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    let mut store = crate::paths::load();
    if data_dir.is_empty() {
        // Clear → revert to default.
        store.services.remove(&name);
        if let Err(e) = crate::paths::save(&store) {
            return Reply::err_msg("io_error", format!("persist service paths: {e}"));
        }
        return Reply::ok(json!({
            "name": name,
            "data_dir": crate::paths::default_data_dir(&name).to_string_lossy(),
            "source": "default",
        }));
    }
    store.set(&name, &data_dir);
    if let Err(e) = crate::paths::save(&store) {
        return Reply::err_msg("io_error", format!("persist service paths: {e}"));
    }
    Reply::ok(json!({
        "name": name,
        "data_dir": data_dir,
        "source": "override",
    }))
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
    // A service the daemon has explicitly marked crashed (`dead-orphan`) or
    // given up on (`failed`) is inactive regardless of how recent its last
    // heartbeat was. Without this a just-crashed service would classify
    // `active` for up to 90s (its final heartbeat is still fresh), hiding the
    // crash from the dashboard until the heartbeat aged out.
    if matches!(info.state.as_deref(), Some("dead-orphan") | Some("failed")) {
        return "inactive";
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

/// File-modification age in seconds (now − mtime) of the binary at `path`,
/// or `None` when the path is missing / unreadable. Mirrors `heartbeat_age`'s
/// "now minus timestamp" shape so it composes with [`binary_predates_process`].
/// A binary stamped in the future clamps to age 0 (treated as brand new).
fn binary_age_secs(path: &Path) -> Option<f64> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Some(age)
}

/// F1 staleness guard (pure core): a running service is on a STALE binary when
/// the live process has been up *longer* than its on-disk binary has existed —
/// i.e. the binary was rebuilt after the process started. Both inputs are ages
/// in seconds (now − timestamp); the grace window absorbs clock jitter. A
/// non-finite age (missing/garbled `started_at`, unreadable binary) is never
/// stale.
fn binary_predates_process(binary_age_s: f64, process_age_s: f64) -> bool {
    binary_age_s.is_finite()
        && process_age_s.is_finite()
        && process_age_s > binary_age_s + STALE_BINARY_GRACE_S
}

/// Flag each *running* service whose on-disk binary is newer than the live
/// process (F1). Impure — stats the resolved binary and reads the clock — so
/// it is kept out of the pure, filesystem-free [`shape_service_list`]. The
/// process age comes from `started_at` (process uptime), compared against the
/// age of the binary that would spawn today.
///
/// Covers **both** build roots: a core in-tree binary
/// ([`rust_binary_path`], under `rust/`) and a discovered out-of-tree
/// sibling's dropped artifact ([`crate::state::services::sibling_binary_path`],
/// beside `Services/<svc>/manifest.json`). This is the F1 tie for the
/// out-of-tree model — `cargo xtask build-all` stages a fresh sibling
/// artifact, and `service.list` then reports `stale:true` until the sibling
/// is bounced, exactly the deploy-gap gate W0 uses for core services.
fn annotate_staleness(infos: &mut [ServiceInfo]) {
    // Resolved once so a sibling's beside-manifest binary can be located
    // when the in-tree resolver misses (siblings don't live under rust/).
    let siblings = registry::discovered_bucket_services();
    for info in infos.iter_mut() {
        if !info.running {
            continue;
        }
        let process_age = heartbeat_age(info.started_at.as_deref());
        let path = match rust_binary_path(&info.name) {
            Some(p) => Some(p),
            None => siblings
                .iter()
                .find(|d| d.name == info.name)
                .and_then(|d| crate::state::services::sibling_binary_path(&d.folder, &info.name)),
        };
        let Some(path) = path else {
            continue;
        };
        let Some(binary_age) = binary_age_secs(&path) else {
            continue;
        };
        info.stale_binary = binary_predates_process(binary_age, process_age);
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
            // The manifest's lifecycle state verbatim. `bucket`/`status` is the
            // coarse active/stale/inactive tier; this is the precise state so
            // the dashboard can tell a crashed-and-retrying service
            // (`dead-orphan`) from one the crash-restart breaker gave up on
            // (`failed`). Null for declarative-only entries (no runtime file).
            "lifecycle_state": info.state.clone().map(Value::String).unwrap_or(Value::Null),
            // min_core floor unmet: the service is present but needs a newer
            // Core than is running. Carries the human-readable reason so the GUI
            // shows *why* the feature is unavailable rather than a silent
            // absence. Null when compatible / no floor declared.
            "incompatible_reason": info
                .incompatible_reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
            // F1: the live process is running an out-of-date binary (rebuilt
            // after it started). Distinct from down/inactive — the service is
            // up but predates code it may be asked to serve.
            "stale": info.stale_binary,
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
        service_name::WORKSPACES => start_workspaces().await,
        service_name::N8N => start_n8n().await,
        // Generic arm — an out-of-tree sibling discovered under Services/*.
        // Route to the generic supervision path instead of erroring, so the
        // accept-list is no longer the fixed core array.
        other => match registry::discovered_bucket_services()
            .into_iter()
            .find(|d| d.name == other)
        {
            Some(svc) => start_discovered(&svc).await,
            None => anyhow::bail!("not a daemon-managed service: {name}"),
        },
    }
}

/// Re-spawn a daemon-managed `name` after the crash-restart supervisor
/// observed it exit unexpectedly. Routes through the same [`dispatch_start`]
/// the operator-facing `service.start` uses, so a restart goes through the
/// service's canonical `start_<service>` path (the already-alive guard, the
/// manifest re-write, the spawn-record refresh) rather than a bespoke spawn.
/// Lives here because the name→start mapping is owned by this module; the
/// supervisor ([`crate::state::restart`]) calls it once the backoff elapses.
pub(crate) async fn restart_service(name: &str) -> anyhow::Result<()> {
    dispatch_start(name).await
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
        service_name::WORKSPACES => stop_workspaces().await,
        service_name::N8N => stop_n8n().await,
        // Generic arm — a discovered sibling. `stop_discovered` is the
        // generic graceful teardown and is an idempotent no-op for a name
        // that was never started, so this is safe for any manageable name.
        other => stop_discovered(other).await,
    }
}

/// Walk the service registry and return the dashboard's expected
/// envelope. Mirrors Python's `list_services_action` envelope shape.
async fn service_list_action(_payload: Value) -> Reply {
    let mut infos = registry::list_services();
    annotate_staleness(&mut infos);
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
/// If `name` is a discovered sibling whose declared `min_core` floor the
/// running Core does not meet, return the human-readable incompatibility reason
/// (`None` when the service is unknown, has no floor, or is compatible). Used by
/// [`service_health_action`] to surface "needs a newer Core" instead of a bare
/// "down", so the panel gate shows the real cause.
fn incompatible_sibling_reason(name: &str) -> Option<String> {
    registry::discovered_bucket_services()
        .into_iter()
        .find(|s| s.name == name)
        .and_then(|s| {
            registry::check_core_floor(registry::core_version(), s.min_core.as_deref()).reason()
        })
}

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
    // min_core special-case: a discovered sibling whose declared floor exceeds
    // the running Core never spawned (see state::services::start_discovered), so
    // a plain pipe probe would just report it "down" with no reason. Surface the
    // real cause — "present but needs a newer Core" — so the panel shows *why*,
    // not a misleading "not running / Start" affordance.
    if let Some(reason) = incompatible_sibling_reason(&name) {
        return Reply::ok(json!({
            "name": name,
            "reply": {
                "ok": false,
                "incompatible": true,
                "reason": reason,
            },
        }));
    }
    let reply = send_with_verb(
        &name,
        LIVENESS_METHOD,
        "GET",
        Value::Null,
        HEALTH_PROBE_TIMEOUT,
    )
    .await;
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

/// Ask the live wrapper whether the upstream Ollama daemon is serving.
///
/// Returns `Some(true)`/`Some(false)` when the wrapper answered its pipe
/// (folding in its own `GET /api/tags` probe of 127.0.0.1:11434), and
/// `None` when the wrapper pipe itself is unreachable — so the caller can
/// decline to touch the upstream from here.
async fn ollama_upstream_serving() -> Option<bool> {
    let body = json!({ "action": "ollama.health", "payload": {} });
    let reply = send_with_verb(
        service_name::OLLAMA,
        ACTION_DISPATCH_PATH,
        "POST",
        body,
        OLLAMA_UPSTREAM_PROBE_TIMEOUT,
    )
    .await;
    if !reply.ok {
        return None;
    }
    Some(reply.data.get("upstream").and_then(Value::as_str) == Some("ok"))
}

/// Ensure the external Ollama daemon is reachable for `service.start
/// wylde-ollama`. The wrapper being alive is NOT enough: `service.health`
/// folds in the daemon at 127.0.0.1:11434, which `service.start` never
/// started, so the GUI's "Start wylde-ollama" stub button used to no-op
/// against the already-alive wrapper and the stub never cleared. When the
/// wrapper reports the daemon down, spawn `ollama serve` and wait for it to
/// answer. Returns whether it spawned the daemon.
async fn ensure_ollama_upstream() -> Result<bool, IpcError> {
    match ollama_upstream_serving().await {
        // Wrapper pipe unreachable — nothing to do about upstream here;
        // the caller reports the wrapper status as-is.
        None => Ok(false),
        // Already serving — idempotent no-op.
        Some(true) => Ok(false),
        Some(false) => {
            let bin = crate::state::services::spawn_ollama_serve().map_err(|e| {
                let msg = format!("{e:#}");
                // The helper marks a missing binary so we can hand the GUI a
                // stable, actionable code distinct from a real spawn failure.
                let code = if msg.contains("ollama_not_installed") {
                    "ollama_not_installed"
                } else {
                    "ollama_start_failed"
                };
                IpcError::new(code, msg)
            })?;
            tracing::info!(
                "daemon: spawned upstream `ollama serve` from {}",
                bin.display()
            );
            let started = std::time::Instant::now();
            loop {
                if matches!(ollama_upstream_serving().await, Some(true)) {
                    return Ok(true);
                }
                if started.elapsed() >= OLLAMA_UPSTREAM_START_DEADLINE {
                    return Err(IpcError::new(
                        "ollama_start_timeout",
                        format!(
                            "started `ollama serve` ({}) but the Ollama daemon did not answer \
                             at 127.0.0.1:11434 within {}s",
                            bin.display(),
                            OLLAMA_UPSTREAM_START_DEADLINE.as_secs(),
                        ),
                    ));
                }
                tokio::time::sleep(OLLAMA_UPSTREAM_START_POLL).await;
            }
        }
    }
}

/// Production spawn for a daemon-managed service. Idempotent on
/// already-running. Names that are neither a core service nor a discovered
/// out-of-tree sibling ([`is_manageable`]) get the Python-compatible
/// `not_registered` error envelope.
///
/// `wylde-ollama` is special: once the wrapper is alive it additionally
/// ensures the external Ollama daemon is reachable (see
/// [`ensure_ollama_upstream`]), so the GUI's "Start wylde-ollama" button
/// actually starts the LLM layer instead of no-op'ing against the wrapper.
async fn service_start_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    if !is_manageable(&name) {
        return Reply::err(IpcError::new(
            "not_registered",
            format!("unknown service {name:?}"),
        ));
    }

    // Bring the wrapper process itself up (idempotent on already-running).
    let mut started = false;
    if !is_service_alive(&name) {
        if let Err(e) = dispatch_start(&name).await {
            return Reply::err(IpcError::new(
                "spawn_failed",
                format!("spawn failed: {e:#}"),
            ));
        }
        started = true;
    }

    if name == service_name::OLLAMA {
        return match ensure_ollama_upstream().await {
            Ok(upstream_started) => {
                let pid = service_pid(&name).map(Value::from).unwrap_or(Value::Null);
                Reply::ok(json!({
                    "name": name,
                    "status": "running",
                    "pid": pid,
                    "started": started,
                    "upstream_started": upstream_started,
                }))
            }
            Err(e) => Reply::err(e),
        };
    }

    let pid = service_pid(&name).map(Value::from).unwrap_or(Value::Null);
    Reply::ok(json!({
        "name": name,
        "status": "running",
        "pid": pid,
        "started": started,
    }))
}

/// Production stop for a daemon-managed service. Names outside
/// the manageable set ([`is_manageable`]) return idempotent no-op success — Python
/// emits the same envelope when the name isn't tracked.
async fn service_stop_action(payload: Value) -> Reply {
    let name = match require_name(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };
    if !is_manageable(&name) {
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
    if !is_manageable(&name) {
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
    if !CORE_SERVICES.contains(&name.as_str()) {
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
    use serial_test::serial;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_actions, unregister_action};

    // The action registry is a process-global. Without a guard,
    // parallel tests race each other's register/cleanup pairs and
    // some lookups land between a sibling's cleanup and re-register.
    //
    // NOTE: this guard covers the ACTION REGISTRY only. Tests that also mutate
    // the process-global WYLDE_ROOT / WYLDE_SERVICES need `#[serial]` on top —
    // `state_guard` guards the same env vars from the `state` module with a
    // *different* mutex, and two locks over one resource is no mutual exclusion.
    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    /// Pins BOTH variables that feed service discovery at a known estate, and
    /// restores them on drop — including on a panicking assert, which a manual
    /// save/restore around the assertion would leak.
    ///
    /// Both, not one: `WYLDE_ROOT` selects the estate and `WYLDE_SERVICES`
    /// independently relocates the Services bucket within it. A test that pins
    /// only one still reads the other from the developer's real environment.
    struct RestoreEnv {
        root: Option<std::ffi::OsString>,
        services: Option<std::ffi::OsString>,
    }
    impl RestoreEnv {
        fn pin(root: &std::path::Path) -> Self {
            let saved = Self {
                root: std::env::var_os("WYLDE_ROOT"),
                services: std::env::var_os("WYLDE_SERVICES"),
            };
            std::env::set_var("WYLDE_ROOT", root);
            std::env::remove_var("WYLDE_SERVICES");
            saved
        }
    }
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.root.take() {
                Some(v) => std::env::set_var("WYLDE_ROOT", v),
                None => std::env::remove_var("WYLDE_ROOT"),
            }
            match self.services.take() {
                Some(v) => std::env::set_var("WYLDE_SERVICES", v),
                None => std::env::remove_var("WYLDE_SERVICES"),
            }
        }
    }

    /// Every action `register_with_ipc` binds — kept in one place so the
    /// cleanup and the registration assertion can't drift apart.
    const ALL_ACTIONS: [&str; 14] = [
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
        "updater.get_prefs",
        "updater.set_prefs",
        "paths.get",
        "paths.set",
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

    #[tokio::test]
    async fn paths_get_set_round_trip_through_actions() {
        // paths.set persists an override; paths.get reads it back. With no
        // override, paths.get resolves to the default WyldeData/<svc>/
        // sibling (source "default"). Uses WYLDE_DATA_DIR to redirect the
        // store at a tempdir, under the state/registry guards.
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        cleanup();
        register_with_ipc();

        let tmp = tempfile::TempDir::new().unwrap();
        let saved = std::env::var_os("WYLDE_DATA_DIR");
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());

        // Default first (no override yet).
        let got = dispatch_action(json!({
            "action": "paths.get",
            "payload": {"name": "wylde-images"},
        }))
        .await;
        assert!(got.ok, "paths.get must succeed");
        assert_eq!(got.data["source"], "default");

        // Set an override, then read it back.
        let set = dispatch_action(json!({
            "action": "paths.set",
            "payload": {"name": "wylde-images", "data_dir": "E:/MyLib"},
        }))
        .await;
        assert!(set.ok, "paths.set must succeed");
        assert_eq!(set.data["source"], "override");
        assert_eq!(set.data["data_dir"], "E:/MyLib");

        let got2 = dispatch_action(json!({
            "action": "paths.get",
            "payload": {"name": "wylde-images"},
        }))
        .await;
        assert_eq!(got2.data["source"], "override");
        assert_eq!(got2.data["data_dir"], "E:/MyLib");

        match saved {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
        cleanup();
    }

    #[test]
    fn workspaces_is_a_core_managed_service() {
        // Slice A registration: wylde-workspaces must be in the core
        // daemon-managed set so `service.start` / `service.wake` accept it
        // (rather than `not_registered`) and the no-spawn parity surface
        // reports it. The count is the dashboard's expected-running-services
        // tally — TX S3 added wylde-n8n, bumping it from 11 → 12 (N+1).
        // (Out-of-tree siblings are accepted via is_manageable, not this
        // const — see service.start's discovery-driven gate.)
        assert_eq!(
            CORE_SERVICES.len(),
            12,
            "expected 12 core daemon-managed services after registering wylde-n8n"
        );
        assert!(
            CORE_SERVICES.contains(&service_name::WORKSPACES),
            "wylde-workspaces must be daemon-managed (Slice A)"
        );
        assert!(
            CORE_SERVICES.contains(&service_name::N8N),
            "wylde-n8n must be daemon-managed (TX S3)"
        );
    }

    #[tokio::test]
    async fn service_start_n8n_is_registered_not_unknown() {
        // TX S3 wiring proof: `service.start` for wylde-n8n must route into
        // `dispatch_start` (→ start_n8n) rather than returning the
        // `not_registered` envelope. No binary is built in the unit-test
        // env, so start_n8n is a non-fatal no-op — the point is purely that
        // the name is recognised and the miss is non-fatal (optional tier).
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        // Point the BIN override at a missing path so no real child spawns.
        std::env::set_var("WYLDE_WYLDE_N8N_BIN", "/no/such/n8n/binary");
        let reply = dispatch_action(json!({
            "action": "service.start",
            "payload": {"name": "wylde-n8n"},
        }))
        .await;
        std::env::remove_var("WYLDE_WYLDE_N8N_BIN");

        // Either ok (no-op spawn) — but NEVER the `not_registered` envelope.
        if let Some(err) = reply.error {
            assert_ne!(
                err.code, "not_registered",
                "wylde-n8n must be a registered daemon-managed service"
            );
        }
    }

    #[tokio::test]
    async fn service_start_workspaces_is_registered_not_unknown() {
        // The registration is wired end-to-end: `service.start` for
        // wylde-workspaces must route into `dispatch_start` (→
        // start_workspaces) rather than returning the `not_registered`
        // envelope reserved for names outside the managed set. No binary is
        // built in the unit-test env, so start_workspaces is a non-fatal
        // no-op and the action returns ok — the point is purely that the
        // name is recognised.
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        // Point the BIN override at a missing path so no real child spawns.
        std::env::set_var("WYLDE_WYLDE_WORKSPACES_BIN", "/no/such/workspaces/binary");
        let reply = dispatch_action(json!({
            "action": "service.start",
            "payload": {"name": "wylde-workspaces"},
        }))
        .await;
        std::env::remove_var("WYLDE_WYLDE_WORKSPACES_BIN");

        // Either ok (no-op spawn) — but NEVER the `not_registered` envelope.
        if let Some(err) = reply.error {
            assert_ne!(
                err.code, "not_registered",
                "wylde-workspaces must be a registered daemon-managed service"
            );
        }

        cleanup();
    }

    #[serial]
    #[tokio::test]
    async fn service_start_accepts_discovered_sibling() {
        // The accept-list is discovery-driven, not a fixed array: a sibling
        // dropped into Services/<name>/ must be accepted by `service.start`
        // (routed into start_discovered) rather than rejected with
        // `not_registered`. No binary is staged beside the manifest, so the
        // spawn is a non-fatal no-op — the point is purely that the name is
        // accepted as manageable.
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        cleanup();
        register_with_ipc();

        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = tmp
            .path()
            .join("Services")
            .join("wylde-foo")
            .join("manifest.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            serde_json::to_vec(&json!({
                "name": "wylde-foo",
                "enabled": true,
                "pipe": "wylde-foo",
            }))
            .unwrap(),
        )
        .unwrap();

        // Pin BOTH variables that feed discovery, not just one. This test used
        // to set only WYLDE_ROOT, so an ambient WYLDE_SERVICES relocated the
        // bucket to the developer's REAL Services/ estate instead of `tmp` —
        // `wylde-foo` isn't there, so it failed `not_registered` on every
        // machine with Wylde configured. Green on CI only because CI has
        // neither set.
        let _restore = RestoreEnv::pin(tmp.path());
        let reply = dispatch_action(json!({
            "action": "service.start",
            "payload": {"name": "wylde-foo"},
        }))
        .await;

        if let Some(err) = reply.error {
            assert_ne!(
                err.code, "not_registered",
                "a discovered Services/* sibling must be accepted, not rejected"
            );
        }
        cleanup();
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
    fn binary_predates_process_flags_rebuild_after_start() {
        // Process up for 1h, binary built 1m ago ⇒ binary rebuilt after the
        // process started ⇒ STALE (the §0/§7 wylde-workspaces situation).
        assert!(binary_predates_process(60.0, 3600.0));
        // Binary older than the process (normal: built, then started) ⇒ fresh.
        assert!(!binary_predates_process(3600.0, 60.0));
        // Within the grace window ⇒ not flagged (rebuild-then-start jitter).
        assert!(!binary_predates_process(
            100.0,
            100.0 + STALE_BINARY_GRACE_S - 1.0
        ));
        // Just past grace ⇒ flagged.
        assert!(binary_predates_process(
            100.0,
            100.0 + STALE_BINARY_GRACE_S + 1.0
        ));
        // Missing started_at (infinite process age) is never stale, and an
        // unreadable binary (infinite age) likewise.
        assert!(!binary_predates_process(60.0, f64::INFINITY));
        assert!(!binary_predates_process(f64::INFINITY, 3600.0));
    }

    #[test]
    fn shape_service_list_carries_stale_flag() {
        let mut info = fresh_info("wylde-x", "standard");
        info.running = true;
        info.stale_binary = true;
        let v = shape_service_list(vec![info]);
        assert_eq!(v["services"][0]["stale"], true);
        // A fresh service reports stale=false (default).
        let mut ok = fresh_info("wylde-y", "standard");
        ok.running = true;
        let v = shape_service_list(vec![ok]);
        assert_eq!(v["services"][0]["stale"], false);
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
        let fresh = (now - chrono::Duration::seconds(30))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let aged = (now - chrono::Duration::seconds(150))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let dead = (now - chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

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

        for action in [
            "service.start",
            "service.stop",
            "service.wake",
            "service.health",
        ] {
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

    #[serial]
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
        // KNOWN LOCAL FAILURE, deliberately NOT fixed here (KI-6).
        //
        // `count == 0` means "nothing was discovered to stop". On a machine with
        // an ambient `WYLDE_ROOT` (i.e. any configured Wylde dev box) this comes
        // back 10 and the test fails; unsetting WYLDE_ROOT alone makes it pass,
        // verified by isolation. It fails even when run ALONE, so it is NOT the
        // parallel env race this change fixes.
        //
        // Pinning the env from inside the test does not help — the root is
        // already resolved by the time this body runs — which is why it needs a
        // real fix (inject the root, or stub the teardown steps) rather than an
        // env guard. Out of scope for the race fix; called out in the PR so it
        // isn't mistaken for something this change was supposed to cover.
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

    // ── DEV-ONLY dev.restart_service (full-stack hot-reload) ────────────

    const DEV_RESTART_ACTION: &str = "dev.restart_service";

    /// Clear the dev gate so it can never leak into a sibling test's
    /// `register_with_ipc`. Always paired with `unregister_action` of the
    /// dev verb.
    fn clear_dev_gate() {
        std::env::remove_var(super::DEV_HOTRELOAD_ENV);
        unregister_action(DEV_RESTART_ACTION);
    }

    #[tokio::test]
    async fn dev_restart_service_not_registered_without_gate() {
        // The DEV verb must be invisible in a normal/release daemon: with
        // WYLDE_DEV_HOTRELOAD unset, register_with_ipc must NOT bind it.
        let _g = registry_guard().await;
        clear_dev_gate();
        cleanup();
        register_with_ipc();
        assert!(
            !list_actions().contains(&DEV_RESTART_ACTION.to_string()),
            "dev.restart_service must not be registered without the dev gate"
        );
        cleanup();
        clear_dev_gate();
    }

    #[tokio::test]
    async fn dev_restart_service_registered_with_gate() {
        let _g = registry_guard().await;
        clear_dev_gate();
        cleanup();
        std::env::set_var(super::DEV_HOTRELOAD_ENV, "1");
        register_with_ipc();
        assert!(
            list_actions().contains(&DEV_RESTART_ACTION.to_string()),
            "dev.restart_service must be registered when the dev gate is on"
        );
        cleanup();
        clear_dev_gate();
    }

    #[tokio::test]
    async fn dev_restart_service_refuses_when_gate_cleared() {
        // Defence-in-depth: even if the action is somehow still bound, the
        // handler itself re-checks the gate and refuses with not_dev_mode.
        let _g = registry_guard().await;
        clear_dev_gate();
        cleanup();
        std::env::set_var(super::DEV_HOTRELOAD_ENV, "1");
        register_with_ipc();
        // Clear the gate but leave the handler bound.
        std::env::remove_var(super::DEV_HOTRELOAD_ENV);
        let reply = dispatch_action(json!({
            "action": DEV_RESTART_ACTION,
            "payload": {"name": "wylde-workspaces"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_dev_mode");
        cleanup();
        clear_dev_gate();
    }

    #[tokio::test]
    async fn dev_restart_service_rejects_unknown_name() {
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        clear_dev_gate();
        cleanup();
        std::env::set_var(super::DEV_HOTRELOAD_ENV, "1");
        register_with_ipc();

        let reply = dispatch_action(json!({
            "action": DEV_RESTART_ACTION,
            "payload": {"name": "wylde-parity-bogus"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_registered");

        cleanup();
        clear_dev_gate();
    }

    #[tokio::test]
    async fn dev_restart_service_bounces_managed_service_no_binary() {
        // Happy path with no binary swap: stop (no tracked child → no-op)
        // then start. wylde-workspaces with its BIN override pointed at a
        // missing path resolves to no binary, so start is a non-fatal
        // no-op — the action still returns ok with status "restarted",
        // proving the stop→start composition is wired and that a missing
        // binary doesn't crash the bounce.
        let _g = registry_guard().await;
        let _sg = crate::state::tests::state_guard().await;
        clear_nospawn();
        clear_dev_gate();
        cleanup();
        std::env::set_var(super::DEV_HOTRELOAD_ENV, "1");
        register_with_ipc();

        std::env::set_var("WYLDE_WYLDE_WORKSPACES_BIN", "/no/such/workspaces/binary");
        let reply = dispatch_action(json!({
            "action": DEV_RESTART_ACTION,
            "payload": {"name": "wylde-workspaces"},
        }))
        .await;
        std::env::remove_var("WYLDE_WYLDE_WORKSPACES_BIN");

        assert!(reply.ok, "expected ok, got {reply:?}");
        assert_eq!(reply.data["name"], "wylde-workspaces");
        assert_eq!(reply.data["status"], "restarted");
        assert_eq!(reply.data["swapped"], false);

        cleanup();
        clear_dev_gate();
    }

    #[test]
    fn swap_staged_binary_copies_src_over_dest() {
        // Pure helper: with the BIN override pointing at a (nonexistent)
        // dest under a temp dir, swap_staged_binary copies the src bytes
        // into place, creating the stage dir as needed.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("fresh.exe");
        std::fs::write(&src, b"NEWBYTES").unwrap();
        let dest = tmp.path().join("stage").join("wylde-swaptest.exe");
        std::env::set_var("WYLDE_WYLDE_SWAPTEST_BIN", &dest);

        let r = swap_staged_binary("wylde-swaptest", src.to_str().unwrap());
        std::env::remove_var("WYLDE_WYLDE_SWAPTEST_BIN");
        assert!(r.is_ok(), "swap failed: {r:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"NEWBYTES");
    }

    #[test]
    fn swap_staged_binary_errors_without_stage_target() {
        std::env::remove_var("WYLDE_WYLDE_NOSTAGE_BIN");
        let err = swap_staged_binary("wylde-nostage", "/whatever")
            .expect_err("expected error when BIN override is unset");
        assert!(
            err.to_string().contains("WYLDE_WYLDE_NOSTAGE_BIN"),
            "error should name the missing override: {err}"
        );
    }

    #[test]
    fn swap_staged_binary_errors_on_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("wylde-misssrc.exe");
        std::env::set_var("WYLDE_WYLDE_MISSSRC_BIN", &dest);
        let err = swap_staged_binary("wylde-misssrc", "/no/such/source/binary")
            .expect_err("expected error when source is missing");
        std::env::remove_var("WYLDE_WYLDE_MISSSRC_BIN");
        assert!(
            err.to_string().contains("source binary does not exist"),
            "error should flag the missing source: {err}"
        );
    }

    #[tokio::test]
    async fn dev_hotreload_enabled_reads_env() {
        // Hold the registry guard: this mutates the process-global dev
        // gate env, which the register-time tests read — serialise so a
        // transient flip can't bind/unbind the verb under them.
        let _g = registry_guard().await;
        std::env::remove_var(super::DEV_HOTRELOAD_ENV);
        assert!(!dev_hotreload_enabled());
        for truthy in ["1", "true", "yes", "on", "ON", "True"] {
            std::env::set_var(super::DEV_HOTRELOAD_ENV, truthy);
            assert!(dev_hotreload_enabled(), "{truthy:?} should enable the gate");
        }
        std::env::set_var(super::DEV_HOTRELOAD_ENV, "0");
        assert!(!dev_hotreload_enabled());
        std::env::remove_var(super::DEV_HOTRELOAD_ENV);
    }
}
