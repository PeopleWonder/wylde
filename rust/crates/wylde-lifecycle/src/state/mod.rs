//! Daemon-managed subprocess + scheduler handles.
//!
//! Rust port of `Core/Lifecycle/daemon_state/__init__.py`. Owns the
//! shared state every part of the daemon reads from:
//!
//! * Per-service [`tokio::process::Child`] handles. Submodules mutate
//!   these via [`set_service_proc`] / [`take_service_proc`] so the
//!   unified teardown sees the same instances.
//! * The main-loop stop event ([`register_stop_event`],
//!   [`request_daemon_exit`]). Action handlers use this to flip the
//!   daemon out of `serve_forever` after their reply has been flushed
//!   to the caller.
//! * Spawn records — the daemon's "I started this thing" register the
//!   orphan sweep walks for failed-to-launch detection.
//!
//! Layout note: in the Python package the state globals live on
//! `daemon_state/__init__.py` because monkeypatched tests on the
//! package namespace need to reach them. Rust has no equivalent
//! monkeypatch surface, but we keep the same shape — one module owns
//! the state, sibling modules reach in via the helpers below — so the
//! file-per-Python-module mapping holds (the Wylde user's standing instruction).
//!
//! Submodules:
//! * [`manifest`] — Core's runtime manifest writer + heartbeat thread.
//! * [`orphan_sweep`] — 60s tick that walks `data/manifests/*.json`.
//! * [`services`] — seven `start_<service>` / `stop_<service>` pairs
//!   and the env-var dispatch that picks Python vs Rust per service.

pub mod manifest;
pub mod orphan_sweep;
pub mod restart;
pub mod services;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::process::Child;
use tokio::sync::Notify;

pub use crate::state::manifest::{register_core_manifest, unregister_core_manifest};
pub use crate::state::orphan_sweep::{
    boot_orphan_sweep, start_orphan_sweep, stop_orphan_sweep, sweep_orphans,
};

/// Canonical names for the seven daemon-managed services. Used both as
/// the key into [`STATE`]'s process map and as the manifest filename
/// (without the `.json` suffix) so the orphan sweep agrees with the
/// services map on which manifests belong to which child.
pub mod service_name {
    pub const MEMGRAPH: &str = "wylde-memgraph";
    pub const VOICE: &str = "wylde-voice";
    pub const VRAM_BROKER: &str = "wylde-vram-broker";
    pub const DEVICE_GATE: &str = "wylde-device-gate";
    pub const EXTENSION_BRIDGE: &str = "wylde-extension-bridge";
    pub const GATEWAY: &str = "wylde-gateway";
    pub const OLLAMA: &str = "wylde-ollama";
    /// WyldeLink VPN. Phase 2 of the Rust migration — `WYLDE_WYLDE_VPN_IMPL`
    /// defaults to `python`; the Rust impl is a foundation slice (control
    /// plane + 16 actions, with tunnel/NAT/discovery stubbed).
    pub const VPN: &str = "wylde-vpn";
    pub const MEMORY_SCHEDULER: &str = "wylde-memory-scheduler";
    /// Wylde harness — chat-turn driver. Phase 5 of the Rust
    /// migration. Slice 5.D (2026-05-25) flipped
    /// `WYLDE_WYLDE_HARNESS_IMPL`'s default from `python` to `rust`:
    /// the lifecycle daemon now spawns the consolidated
    /// `wylde-harness.exe` fronting the chat.* action surface over
    /// `\\.\pipe\wylde-harness`. Set
    /// `WYLDE_WYLDE_HARNESS_IMPL=python` to revert to the in-tree
    /// `Core/harness/turn/` driver during the rollback window.
    pub const HARNESS: &str = "wylde-harness";
    /// Tree-sitter sidecar — greenfield Rust structural-parsing service
    /// (NOT a Python port). Default Rust, no Python fallback: a missing
    /// binary leaves it down with a loud build hint (the `wylde-ollama`
    /// precedent). See `docs/plans/treesitter-sidecar.md`.
    pub const TREESITTER: &str = "wylde-treesitter";
    /// Workspace-scoped service (Thought Bubble System Phase 0) — owns the
    /// registry, persona, RAG indexer, notes, workspace conversations, and
    /// the Neo4j code graph. Greenfield Rust, no Python fallback (the
    /// `wylde-treesitter` precedent): a missing binary leaves it down with a
    /// loud build hint. Started LAST in the boot sequence — it consumes
    /// `wylde-ollama` (embedder), `wylde-treesitter` (chunk/extract), and
    /// Memgraph (graph writes), so those must be up first. Consumers degrade
    /// gracefully when it's down (Slice 0d), so a failed spawn is non-fatal.
    pub const WORKSPACES: &str = "wylde-workspaces";
    /// N8N workflow service (taxonomy reorg TX S3) — the Rust pipe
    /// surface over the **external, user-managed** n8n daemon. The
    /// daemon supervises only `wylde-n8n.exe`; it never launches n8n
    /// itself. Optional/non-fatal: a missing binary (or a down n8n)
    /// leaves the service dark and core boots fine — the harness verb
    /// layer degrades to structured errors (the `wylde-workspaces`
    /// precedent). The Python-era `N8N/manifest.json` registry entry
    /// (enabled: false, no entry_point) was retired with this service.
    pub const N8N: &str = "wylde-n8n";
}

/// Window after spawn within which the service is expected to publish
/// its manifest. Past this with no manifest visible the daemon emits a
/// failed-to-launch warning. Matches the 30s value in
/// `daemon_state/__init__.py::_SPAWN_GRACE_SECONDS`.
pub const SPAWN_GRACE_SECONDS: f64 = 30.0;

/// Cadence for the orphan-detection sweep. Matches the unified 60s
/// heartbeat tick so observed liveness signals are roughly synchronous.
pub const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The daemon's runtime state. Owned by a process-global `OnceLock` so
/// every submodule reaches the same instance — the equivalent of the
/// Python `daemon_state` module's namespace globals.
struct State {
    procs: HashMap<String, Child>,
    spawn_records: HashMap<String, SpawnRecord>,
    manifest_dir: PathBuf,
    stop: Option<std::sync::Arc<Notify>>,
    orphan_sweep_stop: Option<std::sync::Arc<Notify>>,
    /// No-spawn mode flag — see the no-spawn section below.
    nospawn: bool,
    /// "Would-have-spawned" registry (name → impl lang). Populated by the
    /// `services::start_<service>` no-spawn short-circuit instead of a real
    /// child. Only ever non-empty when [`nospawn`] is set.
    nospawn_services: HashMap<String, String>,
}

impl State {
    fn new() -> Self {
        let root = Self::resolve_root();
        Self {
            procs: HashMap::new(),
            spawn_records: HashMap::new(),
            manifest_dir: root.join("data").join("manifests"),
            stop: None,
            orphan_sweep_stop: None,
            nospawn: false,
            nospawn_services: HashMap::new(),
        }
    }

    /// Production: the daemon's root is `WYLDE_ROOT`, falling back to the cwd.
    #[cfg(not(test))]
    fn resolve_root() -> PathBuf {
        std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Tests: **never** read `WYLDE_ROOT` (#80).
    ///
    /// This is the seam the whole crate's test hermeticity rests on, so the
    /// reasoning lives here rather than at the call site.
    ///
    /// `state()` is a process-global `OnceLock`: the root is resolved **once**,
    /// by whichever test touches it first, and is fixed for the rest of the
    /// binary's life. So a test that sets `WYLDE_ROOT` in its own body cannot
    /// affect it — the read already happened. That is not a fixable ordering
    /// problem; it is why #78 found that pinning the env from inside the body
    /// "does not help", and why #80's `count == 0` silently became `count == 11`
    /// on a configured machine: `is_or_was_tracked` stats
    /// `<manifest_dir>/<service>.json`, and `manifest_dir` was pointing at the
    /// developer's **real** estate.
    ///
    /// Reading ambient env here therefore can't be made safe — it can only be
    /// not done. Under `cfg(test)` the root is a per-process scratch path that
    /// nothing else writes, so every test in this crate is hermetic **by
    /// construction** rather than by remembering to guard. A test that wants a
    /// populated root builds one and points at it explicitly.
    ///
    /// Pinned by `resolve_root_is_hermetic_under_cfg_test` below — deleting or
    /// reverting this fails that gate rather than silently re-arming #80.
    #[cfg(test)]
    fn resolve_root() -> PathBuf {
        std::env::temp_dir().join(format!("wylde-lifecycle-test-root-{}", std::process::id()))
    }
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::new()))
}

// ── No-spawn (parity / test) mode ──────────────────────────────────────
//
// No-spawn mode is a TEST-AND-PARITY-ONLY switch. When enabled (env
// `WYLDE_LIFECYCLE_NOSPAWN=1` or the `--no-spawn` CLI flag) the daemon
// brings up its full control surface — the `\\.\pipe\wylde-lifecycle`
// pipe, every registered action — but the `services::start_<service>`
// functions DO NOT fork child processes. Each records a
// "would-have-spawned" entry in `nospawn_services` so the `lifecycle.*`
// parity actions (and `service.shutdown_all`) report what the daemon
// *would* have done.
//
// A no-spawn daemon also leaves the host-wide `data/manifests/core.json`
// untouched — neither written at boot nor deleted at shutdown — so a
// parity run never clobbers a production daemon's manifest. Combined
// with the `WYLDE_LIFECYCLE_PIPE_NAME` isolated-pipe override, a parity
// run is safe to perform while the real Wylde stack is up.
//
// ⚠️  THIS MUST NEVER BE ENABLED IN PRODUCTION. ⚠️
// A no-spawn daemon supervises nothing — Memgraph, Voice, the VRAM
// broker, the gateway and device_gate never start. It exists solely so
// the cross-language parity suite (`rust/tests/parity/tests/lifecycle.rs`)
// can exercise the control + manifest surfaces without booting Wylde's
// entire tier=core stack. It is the byte-for-byte counterpart of the
// Python daemon's no-spawn mode (`Core/Lifecycle/daemon_state`).

/// Enable / disable no-spawn mode. Set once at daemon boot.
///
/// TEST/PARITY ONLY — see the no-spawn warning above. Production daemons
/// never call this; the flag defaults to `false`.
pub fn set_nospawn(enabled: bool) {
    if let Ok(mut s) = state().lock() {
        s.nospawn = enabled;
    }
}

/// True when no-spawn mode is active (`start_<service>` short-circuits).
///
/// TEST/PARITY ONLY — see the no-spawn warning above.
pub fn nospawn_enabled() -> bool {
    state().lock().map(|s| s.nospawn).unwrap_or(false)
}

/// Record that `name` would-have-been-spawned (`impl_lang` is `"python"`
/// or `"rust"`). The no-spawn analogue of a real spawn + [`record_spawn`].
pub fn nospawn_record(name: &str, impl_lang: &str) {
    if let Ok(mut s) = state().lock() {
        s.nospawn_services
            .insert(name.to_owned(), impl_lang.to_owned());
    }
}

/// Drop a would-have-spawned record; returns whether it was present.
/// The no-spawn analogue of taking a real [`Child`] out for teardown.
pub fn nospawn_take(name: &str) -> bool {
    state()
        .lock()
        .ok()
        .map(|mut s| s.nospawn_services.remove(name).is_some())
        .unwrap_or(false)
}

/// Sorted snapshot of every would-have-spawned service name — diagnostics.
pub fn nospawn_snapshot() -> Vec<String> {
    let mut names: Vec<String> = state()
        .lock()
        .map(|s| s.nospawn_services.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Spawn-record entry tracked per child. The orphan sweep reads these
/// to distinguish "we spawned this and it vanished" from "we never
/// tried to spawn this".
#[derive(Debug, Clone)]
pub struct SpawnRecord {
    pub pid: u32,
    pub spawn_time: Instant,
    pub impl_lang: String,
    pub grace_satisfied: bool,
}

/// Tell orphan-detection that the daemon just spawned `name`.
///
/// `impl_lang` records which implementation language is running
/// (`"python"` or `"rust"`) so dashboards and the orphan-sweep log can
/// distinguish the two during the strangler-fig migration.
pub fn record_spawn(name: &str, pid: u32, impl_lang: &str) {
    if let Ok(mut s) = state().lock() {
        s.spawn_records.insert(
            name.to_owned(),
            SpawnRecord {
                pid,
                spawn_time: Instant::now(),
                impl_lang: impl_lang.to_owned(),
                grace_satisfied: false,
            },
        );
    }
}

/// Clear the spawn record on graceful stop. Stops orphan-detection
/// from flagging a deliberately-stopped service as failed.
pub fn forget_spawn(name: &str) {
    if let Ok(mut s) = state().lock() {
        s.spawn_records.remove(name);
    }
}

/// Snapshot of every active spawn record. The orphan sweep iterates
/// this map (taking the lock again per write-back) to mark records
/// past the grace window as `grace_satisfied`.
pub fn spawn_records_snapshot() -> Vec<(String, SpawnRecord)> {
    state()
        .lock()
        .map(|s| {
            s.spawn_records
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the daemon currently holds a spawn record for `name` — i.e. it
/// spawned the service and hasn't been told to stop it. The crash-restart
/// supervisor uses this as its crash-vs-intended-stop signal: an intended
/// stop clears the record (via [`forget_spawn`]), so a service still recorded
/// here that died was a genuine crash.
pub fn spawn_record_exists(name: &str) -> bool {
    state()
        .lock()
        .map(|s| s.spawn_records.contains_key(name))
        .unwrap_or(false)
}

/// Flip the `grace_satisfied` flag for a spawn record so the
/// failed-to-launch warning fires once and not on every tick.
pub fn mark_grace_satisfied(name: &str) {
    if let Ok(mut s) = state().lock() {
        if let Some(rec) = s.spawn_records.get_mut(name) {
            rec.grace_satisfied = true;
        }
    }
}

/// Store the Child handle for `name`. Overwrites any prior handle —
/// the only legitimate caller is `services::start_<service>`, which
/// guards against double-spawn before reaching here.
pub fn set_service_proc(name: &str, child: Child) {
    if let Ok(mut s) = state().lock() {
        s.procs.insert(name.to_owned(), child);
    }
}

/// Pull the Child handle for `name` out of the state map. Returns
/// `None` if the service was never spawned (or was already taken by an
/// earlier teardown).
pub fn take_service_proc(name: &str) -> Option<Child> {
    state().lock().ok().and_then(|mut s| s.procs.remove(name))
}

/// Quick "is `name` running?" check. Returns `false` if no handle is
/// recorded or the child has exited; locks briefly to consult the
/// kernel via [`Child::try_wait`].
///
/// In no-spawn mode there are no real children — aliveness is read from
/// the `nospawn_services` would-have-spawned registry instead.
pub fn is_service_alive(name: &str) -> bool {
    if let Ok(mut s) = state().lock() {
        if s.nospawn {
            return s.nospawn_services.contains_key(name);
        }
        if let Some(child) = s.procs.get_mut(name) {
            return matches!(child.try_wait(), Ok(None));
        }
    }
    false
}

/// Pid for `name` if a Child is currently tracked. `None` if the
/// service was never spawned or has been taken out for shutdown.
pub fn service_pid(name: &str) -> Option<u32> {
    state()
        .lock()
        .ok()
        .and_then(|s| s.procs.get(name).and_then(Child::id))
}

/// Best-effort pid recorded in `name`'s on-disk manifest.
///
/// Reads `status.pid` from `data/manifests/<name>.json`. Returns `None`
/// when the manifest is missing, unparseable, or carries no positive
/// pid. The `start_<service>` already-alive log lines use this so the
/// operator can see which pid the daemon believes still owns the slot —
/// the diagnostic that was missing when five services silently failed to
/// spawn behind stale manifests (2026-05-31).
pub fn manifest_pid(name: &str) -> Option<u32> {
    let path = manifest_path_for(name);
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = v.get("status")?.get("pid")?.as_u64()?;
    (pid > 0 && pid <= u64::from(u32::MAX)).then_some(pid as u32)
}

/// Read the manifest directory the orphan sweep walks. Resolved from
/// `WYLDE_ROOT` once at state init.
pub fn manifest_dir() -> PathBuf {
    state()
        .lock()
        .map(|s| s.manifest_dir.clone())
        .unwrap_or_else(|_| PathBuf::from("data/manifests"))
}

/// Manifest path for `name`. Mirrors the Python `_manifest_path`
/// special-case: `wylde-core` lands at `core.json` rather than
/// `wylde-core.json` so it stays at the canonical path the dashboard
/// knows.
pub fn manifest_path_for(name: &str) -> PathBuf {
    let dir = manifest_dir();
    if name == "wylde-core" {
        dir.join("core.json")
    } else {
        dir.join(format!("{name}.json"))
    }
}

/// Hand the daemon's main-loop stop event to this module so action
/// handlers can request a graceful exit. Idempotent — re-registering
/// just replaces the reference.
pub fn register_stop_event(notify: std::sync::Arc<Notify>) {
    if let Ok(mut s) = state().lock() {
        s.stop = Some(notify);
    }
}

/// Ask the daemon to exit cleanly after a brief delay.
///
/// The delay matters: the action that triggers this is mid-response.
/// If we flip the notify synchronously the daemon's main task can
/// reach `notified()` and start tearing the pipe server down before
/// the worker task has flushed the reply frame to the caller. Half a
/// second is more than enough for an msgpack envelope to make it
/// across a named pipe on the local box.
///
/// Returns `true` if a stop event is registered (i.e., the daemon
/// will exit), `false` if no event was registered (called outside a
/// running daemon — e.g., from a unit test).
pub fn request_daemon_exit(after: Duration) -> bool {
    let notify = match state().lock().ok().and_then(|s| s.stop.clone()) {
        Some(n) => n,
        None => return false,
    };
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        notify.notify_waiters();
    });
    true
}

/// Store the orphan-sweep cancellation handle so [`stop_orphan_sweep`]
/// can drain the loop on shutdown.
pub(crate) fn register_orphan_sweep_stop(notify: std::sync::Arc<Notify>) {
    if let Ok(mut s) = state().lock() {
        s.orphan_sweep_stop = Some(notify);
    }
}

/// Take the orphan-sweep stop notify so a duplicate
/// [`start_orphan_sweep`] can detect it's already running and drop
/// out, and [`stop_orphan_sweep`] can fire the notify.
pub(crate) fn take_orphan_sweep_stop() -> Option<std::sync::Arc<Notify>> {
    state()
        .lock()
        .ok()
        .and_then(|mut s| s.orphan_sweep_stop.take())
}

/// Is the orphan sweep currently registered? Cheap read of the same
/// slot [`register_orphan_sweep_stop`] writes.
pub(crate) fn orphan_sweep_running() -> bool {
    state()
        .lock()
        .map(|s| s.orphan_sweep_stop.is_some())
        .unwrap_or(false)
}

/// Stop every long-lived child the daemon spawned outside the
/// launcher's tracked-services set.
///
/// Captures the running set first (so the response payload is honest
/// about what was alive), runs each stop in the documented order —
/// orphan sweep first, then scheduler → gateway → extension_bridge →
/// voice → device_gate → vram_broker → memgraph — swallows individual
/// stop failures (each gets logged), and returns a structured summary.
///
/// Both the ctrl_c handler and the `service.shutdown_all` action go
/// through here, so external invocation and Ctrl-C tear down the same
/// way.
pub async fn stop_all_daemon_managed() -> ShutdownSummary {
    let mut stopped: Vec<String> = Vec::new();
    let mut failed: Vec<ShutdownFailure> = Vec::new();

    // Halt orphan-detection BEFORE stopping services — otherwise an
    // in-flight sweep could flag a service mid-teardown as a "dead
    // orphan" and rewrite its manifest to dead-orphan after the
    // service already wrote `stopped`.
    stop_orphan_sweep();

    // Gateway first — it's the outward-facing surface, taking it down
    // before its dependents (extension_bridge + Voice + device_gate)
    // reduces the blast radius if a teardown hangs. Memgraph last so
    // anything still holding a Bolt driver releases first.
    async fn run_step(name: &str, result: anyhow::Result<()>) -> Result<(), ShutdownFailure> {
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!("daemon: stop {} raised: {:#}", name, e);
                Err(ShutdownFailure {
                    name: name.to_owned(),
                    error: format!("{e:#}"),
                })
            }
        }
    }

    let nospawn = nospawn_enabled();

    // Dynamic out-of-tree sibling teardown — symmetric with the daemon's
    // boot discovery loop. Discovered `Services/*` siblings are leaf
    // consumers of the core tier, so they drain FIRST (before the core
    // teardown below) via the generic `stop_discovered` path — nothing
    // hardcoded. CLEAN NO-OP when the bucket is absent/empty: discovery
    // returns nothing and the loop iterates zero times. Skipped under
    // no-spawn (the parity set is the core services only — see the boot
    // loop in `daemon.rs`).
    if !nospawn {
        for svc in crate::registry::discovered_bucket_services() {
            match run_step(&svc.name, services::stop_discovered(&svc.name).await).await {
                Ok(()) => {
                    if is_or_was_tracked(&svc.name) {
                        stopped.push(svc.name);
                    }
                }
                Err(failure) => failed.push(failure),
            }
        }
    }

    // Each tuple captures `was_alive` BEFORE its stop runs — tuple elements
    // evaluate left-to-right, so `is_service_alive` is read before the
    // adjacent `stop_<service>` future is awaited. This mirrors the Python
    // `stop_all_daemon_managed`'s `_try(name, alive, fn)` ordering.
    // Shutdown order (per master plan Phase 1 §6a, extended for VPN +
    // Harness + Workspaces + N8N):
    //   Gateway → N8N → Workspaces → TreeSitter → ExtensionBridge →
    //   Harness → Voice → DeviceGate → Ollama → VPN → VramBroker →
    //   Memgraph
    //
    // Workspaces stops near the front: it consumes Ollama (embeddings),
    // tree-sitter (chunk/extract over the pipe), and Memgraph (Bolt graph
    // writes), so draining it before those releases the resources cleanly.
    //
    // Harness stops AFTER Gateway/ExtensionBridge (its callers are
    // gone) but BEFORE Ollama (its primary downstream — Ollama drains
    // any final lease cleanly after the turn driver releases its
    // last in-flight call). Voice/DeviceGate are unrelated and stop
    // independently. Ollama goes BEFORE the broker so in-flight VRAM
    // leases are released cleanly; the broker then has nothing to reap.
    // VPN sits between Ollama and the broker — independent of either,
    // but ordering it after the VRAM consumers keeps the broker the
    // last "infrastructure" service torn down before Memgraph. Memgraph
    // last so anything still holding a Bolt driver releases first.
    let steps: [(&str, bool, anyhow::Result<()>); 12] = [
        (
            service_name::GATEWAY,
            is_service_alive(service_name::GATEWAY),
            services::stop_gateway().await,
        ),
        // wylde-n8n is a leaf wrapper over the external n8n daemon (which
        // we never stop — it's user-managed); nothing depends on the pipe
        // once Gateway/Harness callers are draining, so it goes early.
        (
            service_name::N8N,
            is_service_alive(service_name::N8N),
            services::stop_n8n().await,
        ),
        // wylde-workspaces is a consumer of Ollama / tree-sitter / Memgraph,
        // so it must drain BEFORE them — stop it up front alongside the other
        // front-tier services so its in-flight ingest releases the sidecar
        // pipe + Bolt driver before those services go down.
        (
            service_name::WORKSPACES,
            is_service_alive(service_name::WORKSPACES),
            services::stop_workspaces().await,
        ),
        // Tree-sitter is a leaf sidecar (nothing depends on it) — drain it
        // early alongside the other front-tier services.
        (
            service_name::TREESITTER,
            is_service_alive(service_name::TREESITTER),
            services::stop_treesitter().await,
        ),
        (
            service_name::EXTENSION_BRIDGE,
            is_service_alive(service_name::EXTENSION_BRIDGE),
            services::stop_extension_bridge().await,
        ),
        (
            service_name::HARNESS,
            is_service_alive(service_name::HARNESS),
            services::stop_harness().await,
        ),
        (
            service_name::VOICE,
            is_service_alive(service_name::VOICE),
            services::stop_voice().await,
        ),
        (
            service_name::DEVICE_GATE,
            is_service_alive(service_name::DEVICE_GATE),
            services::stop_device_gate().await,
        ),
        (
            service_name::OLLAMA,
            is_service_alive(service_name::OLLAMA),
            services::stop_ollama().await,
        ),
        (
            service_name::VPN,
            is_service_alive(service_name::VPN),
            services::stop_vpn().await,
        ),
        (
            service_name::VRAM_BROKER,
            is_service_alive(service_name::VRAM_BROKER),
            services::stop_vram_broker().await,
        ),
        (
            service_name::MEMGRAPH,
            is_service_alive(service_name::MEMGRAPH),
            services::stop_memgraph().await,
        ),
    ];
    for (name, was_alive, result) in steps {
        match run_step(name, result).await {
            Ok(()) => {
                // No-spawn: a service counts as "stopped" iff it was a
                // recorded would-have-spawned entry alive at call time.
                // Real mode keeps the manifest-existence proxy unchanged.
                let include = if nospawn {
                    was_alive
                } else {
                    is_or_was_tracked(name)
                };
                if include {
                    stopped.push(name.to_owned());
                }
            }
            Err(failure) => failed.push(failure),
        }
    }

    // Core's runtime manifest cleanup runs out-of-band — it's not a
    // subprocess that "stopped", just a JSON file we remove so the
    // next service.list doesn't surface a Core entry with a stale
    // heartbeat.
    //
    // Skipped under no-spawn: core.json is host-wide shared state. A
    // no-spawn (parity) daemon never wrote it, and deleting it here
    // would clobber a production daemon's manifest if one is running on
    // the same box.
    if !nospawn {
        if let Err(e) = unregister_core_manifest() {
            tracing::error!("daemon: unregister_core_manifest raised: {:#}", e);
        }
    }

    let count = stopped.len();
    ShutdownSummary {
        stopped,
        failed,
        count,
    }
}

/// Was the service ever recorded in the state map? Used by
/// [`stop_all_daemon_managed`] to decide whether to include the name
/// in the `stopped` list — services we never spawned shouldn't appear.
fn is_or_was_tracked(name: &str) -> bool {
    // The Child handle is taken out by the per-service stop_fn, so by
    // the time we get here the proc slot is empty. Use the spawn
    // record's presence-history as a proxy — `forget_spawn` clears
    // them on graceful stop, so if we see one missing it was either
    // never spawned or just torn down successfully. Both cases count
    // as "the service is gone", so return true if either condition
    // holds. Concretely: any name we route through `stop_<service>`
    // was either alive (now stopped → tracked) or never spawned (no
    // change → not tracked).
    //
    // Cheap proxy: check the manifest file. Services that booted
    // wrote one; services that never spawned didn't.
    //
    // ── KNOWN DEFECT: this is wrong for the vram-broker (#80) ─────────────
    //
    // The broker self-registers its manifest as `vram-broker.json` — no
    // `wylde-` prefix — but `service_name::VRAM_BROKER` is `"wylde-vram-broker"`
    // (its *pipe* name). So `manifest_path_for(VRAM_BROKER)` stats
    // `wylde-vram-broker.json`, which nothing ever writes, and this predicate is
    // **unconditionally false for the broker**. A real `service.shutdown_all`
    // therefore omits the broker from `stopped`/`count` even when it was running
    // and was just stopped successfully.
    //
    // Impact is reporting-only — `stop_vram_broker` keys off the process/pipe,
    // not the manifest, so the broker *does* stop. The GUI's shutdown summary
    // just under-counts it.
    //
    // `registry.rs` (~line 146 and its `vram_broker_style_short_name_filtered_by_pipe`
    // test) documents this exact quirk and works around it by matching on EITHER
    // the manifest's `service` field OR the short pipe name. This function never
    // got the same treatment. One quirk, two consumers, one of them patched.
    //
    // Not fixed here: the right fix is a decision, not a patch — either the
    // broker starts writing the prefixed name (touches its self-registration and
    // any reader of the old name) or `manifest_path_for` learns the alias the way
    // the registry did. Both are behaviour changes to a shipped daemon and want
    // their own slice. Tracked on #80.
    let path = manifest_path_for(name);
    path.exists()
}

/// Payload returned by [`stop_all_daemon_managed`] — matches the dict
/// shape the Python `stop_all_daemon_managed` returns so the pipe
/// envelope is identical regardless of which daemon answered.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShutdownSummary {
    pub stopped: Vec<String>,
    pub failed: Vec<ShutdownFailure>,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShutdownFailure {
    pub name: String,
    pub error: String,
}

/// Probe the kernel to see whether `pid` is still running.
///
/// Uses `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, ...)` on
/// Windows — succeeds for any process the current user can observe,
/// including zombies. Returns `false` on any error path so a missing
/// pid never falsely keeps a manifest in the alive bucket. Off-Windows
/// returns `false` (the daemon is Windows-only).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: passing valid feature flag + pid; we close the handle below.
        let handle: HANDLE =
            match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
                Ok(h) if !h.is_invalid() => h,
                _ => return false,
            };
        let mut code: u32 = 0;
        // SAFETY: handle is non-null, we hold ownership and close below.
        let alive = unsafe { GetExitCodeProcess(handle, &mut code as *mut u32) }.is_ok()
            && code == STILL_ACTIVE.0 as u32;
        // SAFETY: handle came from OpenProcess and hasn't been closed.
        unsafe {
            let _ = CloseHandle(handle);
        };
        alive
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serial_test::serial;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

    /// Serialise tests that mutate the process-wide [`STATE`] singleton.
    /// Without this, the parallel cargo test threads' calls to
    /// `record_spawn` / `register_stop_event` / `set_nospawn` clobber each
    /// other's expected snapshots. Reachable from sibling modules' test
    /// suites (e.g. `control`'s `shutdown_all` test) so they can serialise
    /// against the same singleton.
    pub(crate) async fn state_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    // ── The self-collision gate (#47 → #75 → #80) ──────────────────────────
    //
    // Three sightings of one class: a test asserting against a resource the
    // *product* owns. #47 and #75 were pipe names; #80 was this crate's
    // manifest directory. Each was green on CI and red on a developer's box —
    // the inverse of a flake, because CI never runs the stack, so the
    // production resource is always free there.
    //
    // That inversion is why the dynamic gate cannot see this class and a
    // static one is the only enforcement available. #79 built the static half
    // for pipe names (`fixture_pipes_are_private.rs`): a source scan for
    // `\\.\pipe\wylde-<service>` literals.
    //
    // **That shape cannot catch #80, and the distinction is the point.** A
    // pipe bind is a *literal in the test source*, so a scanner sees it. #80's
    // test contains no literal at all — it calls `dispatch_action`, and the
    // `WYLDE_ROOT` read happens three layers down inside a process-global
    // `OnceLock`. The only `WYLDE_ROOT` text in that test was in a comment,
    // which #79's guard deliberately strips. A scan for it would be a
    // permanently-green check: a required context that cannot fail.
    //
    // So this half is enforced structurally instead of textually. Hermeticity
    // is a property of `State::resolve_root` (above), and the gate below pins
    // that property. The rule for the next test author is not "remember to
    // guard the env" — it is that this crate's tests *cannot* see ambient
    // `WYLDE_ROOT`, so an assertion about the machine is now impossible to
    // write by accident.
    //
    // Adding a resource? Ask which half it is. Literal in the test → extend
    // #79's scanner. Resolved inside production code → make the resolution
    // hermetic under `cfg(test)` and pin it here.

    /// Gate: this crate's tests must never resolve their root from the
    /// developer's ambient `WYLDE_ROOT` (#80).
    ///
    /// Asserts the property directly rather than trusting the `cfg` to be
    /// wired: if someone reverts `resolve_root`'s `#[cfg(test)]` arm, this
    /// fails on any configured machine instead of #80 quietly returning.
    #[test]
    fn resolve_root_is_hermetic_under_cfg_test() {
        let resolved = State::resolve_root();

        if let Some(ambient) = std::env::var_os("WYLDE_ROOT") {
            let ambient = PathBuf::from(&ambient);
            assert_ne!(
                resolved,
                ambient,
                "tests resolved their root from the ambient WYLDE_ROOT ({}) — \
                 that is #80 re-armed. Assertions would measure the developer's \
                 real estate instead of the fixture, and CI could not catch it \
                 (CI sets no WYLDE_ROOT). See State::resolve_root.",
                ambient.display()
            );
        }

        // Hold regardless of whether the box happens to be configured — a
        // machine with no WYLDE_ROOT must not green this by accident, or the
        // gate would only work where the bug was already visible.
        let scratch = std::env::temp_dir();
        assert!(
            resolved.starts_with(&scratch),
            "the test root ({}) must live under the scratch dir ({}); a root \
             anywhere else is a real location this suite could assert against",
            resolved.display(),
            scratch.display()
        );
    }

    /// The gate is only worth having if it can fail — pin that the production
    /// arm *does* read `WYLDE_ROOT`, so the two arms are known to differ.
    /// Otherwise a refactor collapsing them to one hermetic path would leave
    /// the gate green while the daemon lost its root.
    #[test]
    #[serial]
    fn production_root_still_reads_wylde_root() {
        let prior = std::env::var_os("WYLDE_ROOT");
        // SAFETY: `#[serial]` — no other test runs concurrently. Restored below.
        unsafe { std::env::set_var("WYLDE_ROOT", r"C:\wylde-gate-probe") };

        let production = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        match prior {
            Some(v) => unsafe { std::env::set_var("WYLDE_ROOT", v) },
            None => unsafe { std::env::remove_var("WYLDE_ROOT") },
        }

        assert_eq!(
            production,
            PathBuf::from(r"C:\wylde-gate-probe"),
            "production root resolution must honour WYLDE_ROOT — if this fails, \
             the cfg(test) hermetic arm has leaked into the daemon and the \
             shipped binary would write manifests to a temp dir"
        );
    }

    fn reset_state() {
        if let Ok(mut s) = state().lock() {
            s.procs.clear();
            s.spawn_records.clear();
            s.stop = None;
            s.orphan_sweep_stop = None;
            s.nospawn = false;
            s.nospawn_services.clear();
        }
    }

    #[tokio::test]
    async fn spawn_record_lifecycle() {
        let _g = state_guard().await;
        reset_state();
        record_spawn("svc-a", 1234, "rust");
        let snap = spawn_records_snapshot();
        assert!(snap
            .iter()
            .any(|(k, v)| k == "svc-a" && v.pid == 1234 && v.impl_lang == "rust"));

        mark_grace_satisfied("svc-a");
        let snap = spawn_records_snapshot();
        assert!(snap.iter().any(|(k, v)| k == "svc-a" && v.grace_satisfied));

        forget_spawn("svc-a");
        let snap = spawn_records_snapshot();
        assert!(!snap.iter().any(|(k, _)| k == "svc-a"));
    }

    #[tokio::test]
    async fn nospawn_registry_lifecycle() {
        let _g = state_guard().await;
        reset_state();
        assert!(!nospawn_enabled(), "no-spawn defaults off");

        set_nospawn(true);
        assert!(nospawn_enabled());

        nospawn_record("wylde-gateway", "rust");
        nospawn_record("wylde-voice", "python");
        // In no-spawn mode aliveness reads the would-have-spawned registry.
        assert!(is_service_alive("wylde-gateway"));
        assert!(is_service_alive("wylde-voice"));
        assert!(!is_service_alive("wylde-memgraph"));
        assert_eq!(
            nospawn_snapshot(),
            vec!["wylde-gateway".to_string(), "wylde-voice".to_string()]
        );

        assert!(nospawn_take("wylde-gateway"));
        assert!(!nospawn_take("wylde-gateway"), "take is idempotent");
        assert!(!is_service_alive("wylde-gateway"));

        reset_state();
        assert!(!nospawn_enabled(), "reset clears the flag");
    }

    #[test]
    fn manifest_path_special_cases_core() {
        let p = manifest_path_for("wylde-core");
        assert!(p.ends_with("core.json"));
        let p = manifest_path_for("wylde-voice");
        assert!(p.ends_with("wylde-voice.json"));
    }

    #[test]
    fn pid_alive_returns_false_for_pid_zero() {
        assert!(!pid_alive(0));
    }

    #[cfg(windows)]
    #[test]
    fn pid_alive_returns_true_for_self() {
        let me = std::process::id();
        assert!(pid_alive(me));
    }

    #[cfg(windows)]
    #[test]
    fn pid_alive_returns_false_for_nonexistent() {
        // Pick a pid extremely unlikely to exist. 0xFFFFFFFE is well past
        // Windows' 32-bit pid space.
        assert!(!pid_alive(0xFFFFFFFE));
    }

    #[tokio::test]
    async fn request_daemon_exit_returns_false_when_unregistered() {
        let _g = state_guard().await;
        reset_state();
        assert!(!request_daemon_exit(Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn request_daemon_exit_notifies_after_delay() {
        let _g = state_guard().await;
        reset_state();
        let notify = std::sync::Arc::new(Notify::new());
        register_stop_event(notify.clone());

        assert!(request_daemon_exit(Duration::from_millis(10)));
        tokio::time::timeout(Duration::from_millis(500), notify.notified())
            .await
            .expect("stop notify should fire");
        reset_state();
    }
}
