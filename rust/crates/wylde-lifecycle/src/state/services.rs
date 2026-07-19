//! Seven daemon-managed service start/stop pairs.
//!
//! Rust port of `Core/Lifecycle/daemon_state/_services.py`. Memgraph,
//! Voice, device_gate, vram_broker, extension_bridge, gateway,
//! memory_scheduler. Each `start_<service>` boots the service as a
//! subprocess and records the spawn so orphan-detection knows about
//! it. Each `stop_<service>` sends the OS-appropriate graceful signal,
//! waits for exit, and force-kills on timeout.
//!
//! ## Rust-only (full-Rust cutover R6, 2026-06-10)
//!
//! Every service is Rust; the Python runtime tree was deleted. The
//! strangler-fig `WYLDE_<SERVICE>_IMPL` env vars are still parsed by
//! [`impl_for`] for shape consistency, but `=python` only logs a
//! warning — there is no module left to spawn — so every start path
//! resolves a Rust binary via [`rust_binary_path`], and a missing
//! binary leaves the service down with a loud build hint.
//!
//! Memory scheduler note: the scheduler became a tokio task INSIDE the
//! Rust wylde-harness in slice R2b (`wylde_harness::memory::scheduler`,
//! gated on `WYLDE_HARNESS_SCHEDULER`), so the daemon has no scheduler
//! of its own to start — [`start_memory_scheduler`] just logs that and
//! returns.
//!
//! ## No-spawn mode (test / parity ONLY)
//!
//! When no-spawn mode is active (see [`crate::state::nospawn_enabled`])
//! every `start_<service>` here short-circuits: it records a
//! "would-have-spawned" entry via [`nospawn_record`] and forks NOTHING,
//! and the matching `stop_<service>` clears that record via
//! [`nospawn_take`]. The control + manifest surfaces still come up.
//! ⚠️  Never enable no-spawn in production — a no-spawn daemon supervises
//! nothing. See the no-spawn warning in [`crate::state`].

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::process::{Child, Command};
use wylde_shared::manifest::{HeartbeatHandle, ManifestWriter};

use crate::state::{
    forget_spawn, is_service_alive, manifest_pid, nospawn_enabled, nospawn_record, nospawn_take,
    record_spawn, service_name, service_pid, set_service_proc, take_service_proc,
};

/// `CREATE_NEW_PROCESS_GROUP` from `winbase.h`. Hard-coded so we
/// don't need a `windows` cfg-gated import every callsite — the value
/// is documented and stable.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplLang {
    Python,
    Rust,
}

impl ImplLang {
    pub fn as_str(self) -> &'static str {
        match self {
            ImplLang::Python => "python",
            ImplLang::Rust => "rust",
        }
    }
}

/// Read `WYLDE_<SERVICE>_IMPL` for `service`; default Rust (the Python
/// runtime was deleted in the full-Rust cutover R6).
///
/// The service name `wylde-vram-broker` maps to env var
/// `WYLDE_WYLDE_VRAM_BROKER_IMPL` — dashes become underscores,
/// everything uppercased. Unrecognised values log a warning and fall
/// back to `rust` so a typo can't take a service offline.
pub fn impl_for(service: &str) -> ImplLang {
    impl_for_with_default(service, ImplLang::Rust)
}

/// Same as [`impl_for`] but with an explicit per-service default for
/// when the env var is unset or unrecognised. Every caller passes
/// `Rust` since the R6 deletion wave; the parameter survives so call
/// sites stay explicit about it.
pub fn impl_for_with_default(service: &str, default: ImplLang) -> ImplLang {
    let var = format!("WYLDE_{}_IMPL", service.to_uppercase().replace('-', "_"));
    let raw = match std::env::var(&var) {
        Ok(v) => v,
        Err(_) => return default,
    };
    match raw.to_lowercase().as_str() {
        "rust" => ImplLang::Rust,
        "python" => ImplLang::Python,
        other => {
            tracing::warn!(
                "daemon: {}={:?} is not 'python' or 'rust'; falling back to {}",
                var,
                other,
                default.as_str()
            );
            default
        }
    }
}

/// Resolve the Rust binary for `service` or return `None`.
///
/// Resolution order:
///   1. `WYLDE_<SERVICE>_BIN` override (must point at an existing file).
///   2. Bundled install path `rust/bin/wylde-<stripped>.exe`.
///   3. Cargo release target `rust/target/release/wylde-<stripped>.exe`.
///   4. Cargo debug target `rust/target/debug/wylde-<stripped>.exe`.
///
/// `<stripped>` is `service` with the `wylde-` prefix removed. On
/// non-Windows hosts the `.exe` suffix is dropped (the daemon only
/// runs on Windows in production but tests can exercise the resolver
/// on any platform).
pub fn rust_binary_path(service: &str) -> Option<PathBuf> {
    let stripped = service.strip_prefix("wylde-").unwrap_or(service);
    let override_var = format!("WYLDE_{}_BIN", service.to_uppercase().replace('-', "_"));
    if let Ok(over) = std::env::var(&override_var) {
        let p = PathBuf::from(over);
        return p.exists().then_some(p);
    }

    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let bin_name = format!("wylde-{stripped}{suffix}");
    let root = wylde_root();
    let candidates = [
        root.join("rust").join("bin").join(&bin_name),
        root.join("rust")
            .join("target")
            .join("release")
            .join(&bin_name),
        root.join("rust")
            .join("target")
            .join("debug")
            .join(&bin_name),
    ];
    candidates.iter().find(|p| p.exists()).cloned()
}

fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve the binary for a **discovered out-of-tree sibling** under a
/// bucket (`Services/<name>/`). Unlike [`rust_binary_path`] (which only
/// looks under `rust/`), this resolves the binary the sibling drops *next
/// to its own manifest* — the release artifact `cargo xtask build-all`
/// stages there (plan §4.3).
///
/// Resolution order (mirrors [`rust_binary_path`]'s override-first shape):
///   1. `WYLDE_<NAME>_BIN` override (dev staging) — must point at a file.
///   2. Beside the manifest: `Services/<name>/{wylde-<stripped>,<stripped>}.exe`.
///   3. The sibling's own Cargo target: `Services/<name>/target/{release,debug}/…`
///      (dev convenience before staging).
///
/// `None` when nothing resolves — the caller treats that as a non-fatal
/// "sibling stays down" (core is unaffected).
pub fn sibling_binary_path(folder: &Path, service: &str) -> Option<PathBuf> {
    let override_var = format!("WYLDE_{}_BIN", service.to_uppercase().replace('-', "_"));
    if let Ok(over) = std::env::var(&override_var) {
        let p = PathBuf::from(over);
        return p.exists().then_some(p);
    }
    let stripped = service.strip_prefix("wylde-").unwrap_or(service);
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let names = [
        format!("wylde-{stripped}{suffix}"),
        format!("{stripped}{suffix}"),
    ];
    let dirs = [
        folder.to_path_buf(),
        folder.join("target").join("release"),
        folder.join("target").join("debug"),
    ];
    for dir in &dirs {
        for name in &names {
            let cand = dir.join(name);
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

/// Start a **discovered out-of-tree sibling** service (`Services/<name>/`).
///
/// A thin generalization of [`start_strangler`]: already-alive guard →
/// no-spawn record → resolve the binary beside the manifest
/// ([`sibling_binary_path`]) → [`spawn_rust_binary`] (verbatim: same
/// `WYLDE_ROOT` / `WYLDE_SERVICE_NAME` / data-dir env + `kill_on_drop` +
/// process-group as every core service) → [`record_spawn`] +
/// [`set_service_proc`]. Nothing about the bucket is hardcoded — the
/// daemon supervises whatever discovery returns.
///
/// **Non-fatal:** a missing binary leaves the sibling down with a build
/// hint and returns `Ok` so the rest of the stack is unaffected (the
/// `wylde-workspaces` precedent).
pub async fn start_discovered(svc: &crate::registry::DiscoveredService) -> Result<()> {
    let name = svc.name.as_str();
    if is_service_alive(name) {
        let pid = manifest_pid(name)
            .or_else(|| service_pid(name))
            .unwrap_or(0);
        tracing::info!("{name}: already alive (manifest pid={pid}); skipping spawn");
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(name, ImplLang::Rust.as_str());
        tracing::info!("{name}: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    // min_core compatibility floor — refuse to spawn a sibling that needs a
    // newer Core than is running, LOUDLY (never a silent skip; a silently-absent
    // service is the "panel present but dead" failure class). The reason is also
    // surfaced to the GUI via registry::build_info (service.list) and
    // service.health, so the panel shows *why* rather than just "unavailable".
    let compat =
        crate::registry::check_core_floor(crate::registry::core_version(), svc.min_core.as_deref());
    if let Some(reason) = compat.reason() {
        tracing::error!(
            service = name,
            min_core = svc.min_core.as_deref().unwrap_or(""),
            core = crate::registry::core_version(),
            "refusing to start {name}: {reason}. The service will NOT spawn; its \
             panel will show why. Update Wylde Core, or correct the service's min_core."
        );
        return Ok(());
    }
    let Some(bin) = sibling_binary_path(&svc.folder, name) else {
        tracing::warn!(
            "{name}: no binary found beside its manifest ({}); the sibling will not start — \
             core is unaffected. Build it with `cargo build --release` in its own repo and \
             stage the artifact beside manifest.json (or run `cargo xtask build-all`), or set \
             WYLDE_{}_BIN.",
            svc.folder.display(),
            name.to_uppercase().replace('-', "_"),
        );
        return Ok(());
    };
    let child = spawn_rust_binary(name, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned discovered sibling {name} impl=rust pid={pid}");
    record_spawn(name, pid, ImplLang::Rust.as_str());
    set_service_proc(name, child);
    Ok(())
}

/// Stop a discovered sibling — the generic graceful teardown (the same
/// CTRL_BREAK + wait + force-kill path every other service uses). Keyed on
/// the service name, so the supervision bookkeeping needs no change.
/// Idempotent: an untracked/never-started name is a no-op `Ok`.
pub async fn stop_discovered(name: &str) -> Result<()> {
    stop_service(name, Duration::from_secs(10)).await
}

/// Spawn helper for a Rust service binary. Null Stdio +
/// `CREATE_NEW_PROCESS_GROUP` so signal handling stays uniform across
/// every daemon-managed service.
fn spawn_rust_binary(service_name: &str, rust_bin: &Path) -> Result<Child> {
    tracing::info!(
        "daemon: spawning {} via rust binary {}",
        service_name,
        rust_bin.display()
    );
    // Per-service user-data dir (out-of-tree foundation, plan §3): the
    // persisted override else the default WyldeData/<svc>/ sibling of the
    // repo. Injected as WYLDE_<SVC>_DATA_DIR on EVERY child — the generic
    // contract; a service that owns no library simply ignores it. The path
    // lives in Core config, so it outlives a binary swap, and a change
    // takes effect on the next bounce.
    let data_env = crate::paths::data_dir_env_name(service_name);
    let data_dir = crate::paths::resolve_data_dir(service_name);

    let mut cmd = Command::new(rust_bin);
    cmd.current_dir(wylde_root())
        .env("WYLDE_SERVICE_NAME", service_name)
        .env("WYLDE_ROOT", wylde_root())
        .env(&data_env, &data_dir)
        // Default to `info` so dropped-tracing-subscribers see something.
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    apply_kill_on_drop(&mut cmd);

    cmd.spawn().with_context(|| {
        format!(
            "spawn rust binary {} for {}",
            rust_bin.display(),
            service_name
        )
    })
}

fn apply_kill_on_drop(cmd: &mut Command) {
    // If the daemon exits abnormally (panic) and we haven't called
    // `stop_<service>`, the kernel will drop the Child and tokio
    // signals SIGKILL. That's safer than leaking orphan children.
    cmd.kill_on_drop(true);
}

/// Send the OS-appropriate graceful signal and wait `grace`. On
/// timeout, force-kill and wait another two seconds. Both halves
/// match `_services.py::_stop_<service>`.
async fn graceful_stop(name: &str, mut child: Child, grace: Duration) -> Result<()> {
    let pid = child.id().unwrap_or(0);
    tracing::info!("{}: stopping (pid={})", name, pid);

    #[cfg(windows)]
    {
        // CTRL_BREAK_EVENT works because we spawned with
        // CREATE_NEW_PROCESS_GROUP. CTRL_C_EVENT would also break the
        // daemon parent.
        let _ = send_ctrl_break(pid);
    }
    #[cfg(not(windows))]
    {
        let _ = child.start_kill();
    }

    match tokio::time::timeout(grace, child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(e)) => Err(e).with_context(|| format!("{name}: wait() failed")),
        Err(_) => {
            tracing::warn!(
                "{}: didn't exit within {}s — killing",
                name,
                grace.as_secs_f64()
            );
            let _ = child.kill().await; // wylde-check: discard-result-ok
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await; // wylde-check: discard-result-ok
            Ok(())
        }
    }
}

#[cfg(windows)]
fn send_ctrl_break(pid: u32) -> Result<()> {
    use windows::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    if pid == 0 {
        anyhow::bail!("send_ctrl_break: pid is zero");
    }
    // SAFETY: pid is non-zero; CTRL_BREAK_EVENT delivers to the
    // process group whose group leader has this pid. Always-safe Win32
    // call modulo argument validation.
    let res = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    res.map_err(|e| anyhow::anyhow!("GenerateConsoleCtrlEvent({pid}) failed: {e}"))
}

/// Generic graceful-stop wrapper shared by every `stop_<service>`.
///
/// The per-service stop bodies are byte-identical apart from the grace
/// window, so they collapse to thin wrappers around this: forget the
/// spawn record, clear any no-spawn handle, take the tracked child, and
/// hand it to [`graceful_stop`] with the service's grace. Most services
/// use 10s; Memgraph uses 15s (Neo4j teardown). Keeping the wrappers
/// (rather than calling this directly
/// from `control.rs`) preserves the `pub async fn stop_<service>()`
/// public API the daemon dispatches by name.
async fn stop_service(name: &str, grace: Duration) -> Result<()> {
    forget_spawn(name);
    // Intended stop is sacrosanct: drop any crash-restart bookkeeping so a
    // service the operator stopped is never auto-restarted (and a later
    // legitimate start isn't haunted by a stale crash count / tripped
    // breaker). With the spawn record already gone, a restart pending from a
    // pre-stop crash also aborts at its post-backoff ownership check.
    crate::state::restart::forget(name);
    if nospawn_enabled() {
        nospawn_take(name);
        return Ok(());
    }
    let Some(child) = take_service_proc(name) else {
        return Ok(());
    };
    graceful_stop(name, child, grace).await
}

// ── Strangler-fig start table ─────────────────────────────────────────
//
// Five services share the same start scaffolding: no-spawn short-circuit
// → already-alive guard → spawn → record + track. The only things that
// vary are the service name, the per-service default impl, and the "no
// binary found" warning text — so they live in a table and share one
// generic [`start_strangler`]. All five are rust-only: their Python
// packages were deleted over the migration — device_gate / vram_broker /
// gateway on 2026-06-02, voice in the Phase 11.E cutover, and
// extension_bridge in the full-Rust cutover (2026-06-09) — so a missing
// binary leaves a service down, with no fallback. The unique services
// (memgraph, ollama, harness, vpn) stay hand-written below because
// their control flow genuinely diverges (JVM supervision, hard-fail,
// early-return-no-spawn).

/// One row of the strangler-fig start table.
struct StranglerService {
    /// Canonical service name (e.g. `service_name::VOICE`).
    name: &'static str,
    /// Impl chosen when `WYLDE_<SERVICE>_IMPL` is unset/unrecognised.
    default_impl: ImplLang,
    /// Logged when the Rust impl is requested but no binary resolves.
    missing_binary_warn: &'static str,
}

const STRANGLER_SERVICES: &[StranglerService] = &[
    StranglerService {
        // Collapsed to Rust-only (2026-06-02): the in-tree Python
        // `device_gate` package was DELETED once the Rust port reached
        // parity. The Rust verifier carries its own hash verification
        // (bcrypt / sha-crypt / inline APR1), no interpreter deps. There
        // is no Python fallback — a missing binary leaves the service
        // down rather than spawning a module that no longer exists.
        // `WYLDE_WYLDE_DEVICE_GATE_IMPL` no longer has a `python` target.
        name: service_name::DEVICE_GATE,
        default_impl: ImplLang::Rust,
        missing_binary_warn: "device_gate: no rust binary found; device_gate will not start — the \
             Python device_gate module was removed, so build with `cargo build \
             --release -p wylde-device-gate`",
    },
    StranglerService {
        // Collapsed to Rust-only (2026-06-02): the in-tree Python broker
        // `Core/resource_monitor/` was DELETED after the Rust binary
        // passed a live function test. Only the Rust broker has the
        // Phase-0.5 estimator (a `vram.reserve` with no `bytes` is
        // estimated, not rejected) and DRAM spillover (admits a quantised
        // 27B-class model larger than VRAM on a 16 GB card). There is no
        // Python fallback.
        name: service_name::VRAM_BROKER,
        default_impl: ImplLang::Rust,
        missing_binary_warn: "vram_broker: no rust binary found; vram_broker will not start — the \
             Python Core/resource_monitor package was removed, so build with \
             `cargo build --release -p wylde-vram-broker`",
    },
    StranglerService {
        // Collapsed to Rust-only (full-Rust cutover, 2026-06-09): the
        // Python `Extensions/extension_bridge` importlib dispatcher was
        // DELETED; `wylde-extension-bridge` (MCP-server host) is
        // canonical. Both impls bound the SAME pipe and accepted the
        // SAME `extensions.dispatch` shape (the Rust impl additionally
        // exposes the nine `ext.*` actions + the `ext.events` stream),
        // so Gateway routing is unchanged. The master-plan §11 Q-E1
        // dogfood gate was waived by Aaron with the full-Rust call.
        // `WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL` no longer has a `python`
        // target.
        name: service_name::EXTENSION_BRIDGE,
        default_impl: ImplLang::Rust,
        missing_binary_warn: "extension_bridge: no rust binary found; extension_bridge will not \
             start — the Python Extensions/extension_bridge module was removed, \
             so build with `cargo build --release -p wylde-extension-bridge`",
    },
    StranglerService {
        // Collapsed to Rust-only (2026-06-02): the in-tree Python
        // `Gateway` package was DELETED; the Rust `wylde-gateway` (axum)
        // — a superset of the Python routes — is the canonical
        // ingress/egress. There is no Python fallback;
        // `WYLDE_WYLDE_GATEWAY_IMPL` no longer has a `python` target.
        name: service_name::GATEWAY,
        default_impl: ImplLang::Rust,
        missing_binary_warn: "gateway: no rust binary found; gateway will not start — the Python \
             Gateway package was removed, so build with `cargo build --release \
             -p wylde-gateway`",
    },
    StranglerService {
        // Collapsed to Rust-only (Phase 11.E cutover): the Python `Voice/`
        // tree was DELETED once `wylde-voice` (cpal + ort Whisper/Kokoro +
        // openWakeWord) reached parity and the live session STT/TTS paths
        // moved in-process (orchestrator calls `voice.transcribe` /
        // `voice.synthesize` directly). There is no Python fallback;
        // `WYLDE_WYLDE_VOICE_IMPL` no longer has a `python` target.
        name: service_name::VOICE,
        default_impl: ImplLang::Rust,
        missing_binary_warn: "voice: no rust binary found; voice will not start — the Python \
             Voice package was removed, so build with `cargo build --release \
             -p wylde-voice`",
    },
];

fn strangler_def(name: &str) -> &'static StranglerService {
    STRANGLER_SERVICES
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no StranglerService def for {name}"))
}

/// Generic two-impl start path for the table-driven services. Each
/// `start_<service>` is a thin wrapper that looks up its def and
/// delegates here. Behaviour is identical to the prior hand-written
/// bodies — the per-service warning text and default impl come from the
/// def.
async fn start_strangler(def: &StranglerService) -> Result<()> {
    if is_service_alive(def.name) {
        let pid = manifest_pid(def.name)
            .or_else(|| service_pid(def.name))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            def.name,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        // Every row is rust-only — record `rust` regardless of any stale
        // `=python` override; there is no Python impl to record.
        nospawn_record(def.name, ImplLang::Rust.as_str());
        tracing::info!(
            "{}: NO-SPAWN — would-have-spawned recorded; no child forked",
            def.name
        );
        return Ok(());
    }
    // Rust-only: the Python runtime tree was deleted (full-Rust cutover
    // R6). A missing binary leaves the service down (VPN pattern); an
    // explicit `=python` override is honoured only as a warning.
    if impl_for_with_default(def.name, def.default_impl) == ImplLang::Python {
        tracing::warn!(
            "{}: WYLDE_{}_IMPL=python requested but the Python impl was \
             removed; this service is rust-only",
            def.name,
            def.name.to_uppercase().replace('-', "_"),
        );
    }
    let (child, impl_lang) = match rust_binary_path(def.name) {
        Some(bin) => (spawn_rust_binary(def.name, &bin)?, ImplLang::Rust),
        None => {
            tracing::warn!("{}", def.missing_binary_warn);
            return Ok(());
        }
    };
    let pid = child.id().unwrap_or(0);
    tracing::info!(
        "daemon: spawned {} impl={} pid={}",
        def.name,
        impl_lang.as_str(),
        pid
    );
    record_spawn(def.name, pid, impl_lang.as_str());
    set_service_proc(def.name, child);
    Ok(())
}

// ── Memgraph ──────────────────────────────────────────────────────────
//
// Full-Rust cutover (2026-06-09): the Python wrapper (`Core.Memgraph.run`)
// was retired and the daemon now spawns + supervises the bundled Neo4j
// JVM directly. The wrapper existed for Python-daemon reasons — its own
// signal handler and `sys.path` isolation — that don't apply to a tokio
// child; `kill_on_drop` preserves the "JVM dies with its supervisor"
// guarantee. The legacy pipe surface was already gone (2026-05-26
// direct-Bolt cutover): the harness reads/writes the graph over Bolt
// (neo4rs), so the only job here is JVM lifecycle.
//
// The `wylde-memgraph` manifest + heartbeat are written FROM the daemon
// so the dashboard tile and the orphan sweep keep observing the same
// service identity (Core-constituent semantics; the registry filters it
// from the peer-services list as before).

/// The daemon-held manifest writer + heartbeat for the supervised JVM.
/// `Some` while Memgraph is up; dropped (heartbeat cancelled) on stop.
static MEMGRAPH_MANIFEST: Mutex<Option<(ManifestWriter, HeartbeatHandle)>> = Mutex::new(None);

/// `CREATE_NO_WINDOW` from `winbase.h` — keeps the `cmd /c neo4j.bat`
/// console window from flashing up (mirrors the Python wrapper).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Absolute Wylde root for the Neo4j supervisor.
///
/// `wylde_root()` returns the relative `"."` when `WYLDE_ROOT` is unset —
/// and the production launcher (`launch_wylde.ps1`) does NOT export it, only
/// the dev launcher does. That relative root is fatal for the memgraph
/// spawn specifically: the child `cmd` runs with `current_dir` set to the
/// neo4j subdir, so a root-relative `neo4j.bat` (and `JAVA_HOME` /
/// `NEO4J_HOME`) then resolves *under* that subdir and double-nests —
/// `cmd` dies instantly with "The system cannot find the path specified."
/// and Neo4j never boots (Bolt :7687 stays closed). `bat.exists()` is
/// checked from the daemon CWD so it passes, masking the break. Anchoring
/// to an absolute path makes `current_dir` + the bat arg + the env overlay
/// all resolve regardless of CWD or whether `WYLDE_ROOT` is set.
fn memgraph_root_abs() -> PathBuf {
    let root = wylde_root();
    if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&root))
            .unwrap_or(root)
    }
}

fn memgraph_neo4j_dir() -> PathBuf {
    memgraph_root_abs()
        .join("Core")
        .join("Memgraph")
        .join("vendor")
        .join("neo4j")
}

fn memgraph_jdk_dir() -> PathBuf {
    memgraph_root_abs()
        .join("Core")
        .join("Memgraph")
        .join("vendor")
        .join("jdk")
}

fn memgraph_bolt_port() -> u16 {
    std::env::var("GRAPH_BOLT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7687)
}

/// One cheap TCP probe of the Bolt port (mirrors `_bolt_ready`).
fn memgraph_bolt_ready(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

/// JAVA_HOME / NEO4J_HOME / NEO4J_CONF / PATH for the Neo4j launcher,
/// identical to the Python wrapper's env overlay.
fn memgraph_apply_env(cmd: &mut Command) {
    let jdk = memgraph_jdk_dir();
    let neo4j = memgraph_neo4j_dir();
    let mut paths = vec![jdk.join("bin")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(paths)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default());
    cmd.env("JAVA_HOME", &jdk)
        .env("NEO4J_HOME", &neo4j)
        .env("NEO4J_CONF", neo4j.join("conf"))
        .env("PATH", path);
}

/// Boot the bundled Neo4j JVM under the daemon.
///
/// Skips the spawn when Bolt is already answering (external instance —
/// same as the Python wrapper) and when the vendor launcher is missing
/// (vendor download incomplete; warn and leave the service down).
/// Readiness is observed by a background task so `start_all` isn't
/// blocked for the up-to-120s JVM boot; the harness retries its Bolt
/// connection lazily on first request either way.
pub async fn start_memgraph() -> Result<()> {
    if is_service_alive(service_name::MEMGRAPH) {
        let pid = manifest_pid(service_name::MEMGRAPH)
            .or_else(|| service_pid(service_name::MEMGRAPH))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::MEMGRAPH,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::MEMGRAPH, ImplLang::Rust.as_str());
        tracing::info!("memgraph: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }

    let port = memgraph_bolt_port();
    let bat = memgraph_neo4j_dir().join("bin").join("neo4j.bat");

    let spawned = if memgraph_bolt_ready(port) {
        tracing::info!(
            "memgraph: Neo4j already up on bolt://127.0.0.1:{port} (external instance), \
             skipping spawn"
        );
        false
    } else if !bat.exists() {
        tracing::warn!(
            "memgraph: Neo4j launcher not found at {} — vendor download incomplete? \
             Memgraph will not start.",
            bat.display()
        );
        return Ok(());
    } else {
        // Append-mode JVM log, same location the Python wrapper used.
        // Absolute root (see `memgraph_root_abs`) so the log path is stable
        // even if the daemon CWD differs from the repo root.
        let logs_dir = memgraph_root_abs()
            .join("Core")
            .join("Memgraph")
            .join("logs");
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("create {}", logs_dir.display()))?;
        // Bounded via the shared logging policy: an over-cap file is
        // rolled at open time so this console-capture redirect can't grow
        // forever across restarts. (Neo4j's *own* neo4j.log — the log4j2
        // RollingRandomAccessFile at `server.directories.logs`, 20 MB × 7
        // in conf/user-logs.xml — rotates itself; this is the separate
        // stdout/stderr capture our redirect owns, so we bound it here.)
        let log = wylde_shared::logging::open_rotating_append(
            &logs_dir.join("neo4j.log"),
            wylde_shared::logging::RotationPolicy::from_env(),
        )
        .with_context(|| "open neo4j.log")?;
        let log_err = log.try_clone().with_context(|| "clone neo4j.log handle")?;

        let mut cmd = Command::new("cmd");
        cmd.arg("/c")
            .arg(&bat)
            .arg("console")
            .current_dir(memgraph_neo4j_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        memgraph_apply_env(&mut cmd);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        apply_kill_on_drop(&mut cmd);

        let child = cmd
            .spawn()
            .with_context(|| "spawn cmd /c neo4j.bat console for wylde-memgraph")?;
        let pid = child.id().unwrap_or(0);
        tracing::info!("memgraph: spawned Neo4j launcher (pid={pid}) — boot may take up to 120s");
        record_spawn(service_name::MEMGRAPH, pid, ImplLang::Rust.as_str());
        set_service_proc(service_name::MEMGRAPH, child);
        true
    };

    // Manifest + heartbeat (previously written by the Python wrapper).
    match ManifestWriter::write(
        service_name::MEMGRAPH,
        Some(port),
        "core",
        "Graph data layer (bundled Neo4j via Bolt). Constituent pipe of Core — \
         the registry filters this entry out of the peer services list because \
         Core's rollup manifest covers it.",
        json!({
            "dashboard": { "label": "Memgraph", "icon": "database", "color": "blue" },
        }),
        Some("rust:wylde-lifecycle (in-daemon Neo4j supervisor)"),
    ) {
        Ok(writer) => {
            let hb = writer.start_heartbeat(Duration::from_secs(10));
            *MEMGRAPH_MANIFEST.lock().unwrap_or_else(|p| p.into_inner()) = Some((writer, hb));
        }
        Err(e) => tracing::warn!("memgraph: manifest write failed: {e:#}"),
    }

    // Background readiness probe (1s → ×1.5, capped 5s; budget
    // GRAPH_READY_WAIT_S, default 120) — observational only.
    if spawned {
        let wait_s: u64 = std::env::var("GRAPH_READY_WAIT_S")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120);
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_s);
            let mut interval = Duration::from_secs(1);
            while tokio::time::Instant::now() < deadline {
                if memgraph_bolt_ready(port) {
                    tracing::info!("memgraph: Neo4j ready on bolt://127.0.0.1:{port}");
                    return;
                }
                tokio::time::sleep(interval).await;
                interval = interval.mul_f32(1.5).min(Duration::from_secs(5));
            }
            tracing::warn!(
                "memgraph: Neo4j did not come up within {wait_s}s — supervisor stays up; \
                 the harness retries its Bolt connection lazily on first request"
            );
        });
    }
    Ok(())
}

/// Stop the supervised Neo4j JVM.
///
/// Graceful first — `neo4j.bat stop` pings the running instance over its
/// admin protocol, flushes the WAL, and releases store locks (20s
/// budget) — then a `taskkill /T /F` tree-kill backstop so a runaway
/// `java.exe` is never left behind. Mirrors the Python wrapper's
/// `_stop_neo4j` two-phase teardown.
pub async fn stop_memgraph() -> Result<()> {
    forget_spawn(service_name::MEMGRAPH);
    if nospawn_enabled() {
        nospawn_take(service_name::MEMGRAPH);
        *MEMGRAPH_MANIFEST.lock().unwrap_or_else(|p| p.into_inner()) = None;
        return Ok(());
    }
    let child = take_service_proc(service_name::MEMGRAPH);

    if child.is_some() {
        let bat = memgraph_neo4j_dir().join("bin").join("neo4j.bat");
        if bat.exists() {
            let mut cmd = Command::new("cmd");
            cmd.arg("/c")
                .arg(&bat)
                .arg("stop")
                .current_dir(memgraph_neo4j_dir())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            memgraph_apply_env(&mut cmd);
            #[cfg(windows)]
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
            match cmd.spawn() {
                Ok(mut stopper) => {
                    if tokio::time::timeout(Duration::from_secs(20), stopper.wait())
                        .await
                        .is_err()
                    {
                        tracing::warn!("memgraph: neo4j.bat stop timed out after 20s");
                    }
                }
                Err(e) => tracing::warn!("memgraph: neo4j.bat stop spawn failed: {e}"),
            }
        }
    }

    if let Some(mut child) = child {
        if child.try_wait().ok().flatten().is_none() {
            let pid = child.id().unwrap_or(0);
            tracing::info!("memgraph: tree-killing Neo4j launcher (pid={pid})");
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill") // wylde-check: discard-result-ok
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill().await; // wylde-check: discard-result-ok
            }
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                tracing::warn!("memgraph: Neo4j did not exit cleanly within 5s");
            }
        }
    }

    if let Some((writer, hb)) = MEMGRAPH_MANIFEST
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        drop(hb); // cancel the heartbeat before flipping state
        if let Err(e) = writer.mark_stopped() {
            tracing::warn!("memgraph: mark_stopped failed: {e:#}");
        }
    }
    Ok(())
}

// ── Voice ─────────────────────────────────────────────────────────────
//
// Phase 11a (Slice 11.A — 2026-05-24): strangler-fig dispatch added.
// Slice 11.E+ (2026-05-27): default flipped python → rust once the
// GUI-facing surface (`voice.toggle` / `voice.set_mode` / friends) was
// ported. Phase 11.E cutover: the Python `Voice/` tree was deleted and
// `WYLDE_WYLDE_VOICE_IMPL=python` retired, so voice is now Rust-only —
// same shape as device_gate/vram_broker/gateway/vpn.

pub async fn start_voice() -> Result<()> {
    start_strangler(strangler_def(service_name::VOICE)).await
}

pub async fn stop_voice() -> Result<()> {
    stop_service(service_name::VOICE, Duration::from_secs(10)).await
}

// ── device_gate ───────────────────────────────────────────────────────

pub async fn start_device_gate() -> Result<()> {
    start_strangler(strangler_def(service_name::DEVICE_GATE)).await
}

pub async fn stop_device_gate() -> Result<()> {
    stop_service(service_name::DEVICE_GATE, Duration::from_secs(10)).await
}

// ── VRAM broker ───────────────────────────────────────────────────────

pub async fn start_vram_broker() -> Result<()> {
    start_strangler(strangler_def(service_name::VRAM_BROKER)).await
}

pub async fn stop_vram_broker() -> Result<()> {
    stop_service(service_name::VRAM_BROKER, Duration::from_secs(10)).await
}

// ── Extension bridge ──────────────────────────────────────────────────

/// Boot the extension bridge as a subprocess of the Lifecycle daemon.
///
/// Rust-only since the full-Rust cutover (2026-06-09):
/// `wylde-extension-bridge` (MCP-server host) is the sole impl; the
/// historical Python importlib dispatcher was deleted. It binds the
/// same pipe and accepts the same `extensions.dispatch` action shape
/// the Python impl did, plus nine `ext.*` actions and the
/// `ext.events` stream — Gateway routing is unchanged.
pub async fn start_extension_bridge() -> Result<()> {
    start_strangler(strangler_def(service_name::EXTENSION_BRIDGE)).await
}

pub async fn stop_extension_bridge() -> Result<()> {
    stop_service(service_name::EXTENSION_BRIDGE, Duration::from_secs(10)).await
}

// ── Gateway ───────────────────────────────────────────────────────────

pub async fn start_gateway() -> Result<()> {
    start_strangler(strangler_def(service_name::GATEWAY)).await
}

pub async fn stop_gateway() -> Result<()> {
    stop_service(service_name::GATEWAY, Duration::from_secs(10)).await
}

// ── Ollama ────────────────────────────────────────────────────────────
//
// Greenfield Rust — there is no Python predecessor for `wylde-ollama`.
// The strangler-fig env var pattern is kept for consistency
// (`WYLDE_WYLDE_OLLAMA_IMPL`) but the python branch logs an error
// rather than spawning a non-existent module. Practical default: rust.
pub async fn start_ollama() -> Result<()> {
    if is_service_alive(service_name::OLLAMA) {
        let pid = manifest_pid(service_name::OLLAMA)
            .or_else(|| service_pid(service_name::OLLAMA))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::OLLAMA,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::OLLAMA, ImplLang::Rust.as_str());
        tracing::info!("ollama: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    // wylde-ollama is greenfield Rust. The strangler-fig env var is
    // accepted for shape consistency but Python isn't a valid impl.
    let lang = impl_for(service_name::OLLAMA);
    if lang == ImplLang::Python {
        tracing::warn!(
            "ollama: WYLDE_WYLDE_OLLAMA_IMPL=python but wylde-ollama is greenfield \
             Rust (no Python predecessor); proceeding with rust binary"
        );
    }
    let Some(bin) = rust_binary_path(service_name::OLLAMA) else {
        anyhow::bail!(
            "ollama: rust binary not found (checked WYLDE_WYLDE_OLLAMA_BIN, \
             rust/bin/wylde-ollama.exe, rust/target/release/wylde-ollama.exe, \
             rust/target/debug/wylde-ollama.exe) — build with `cargo build \
             --release -p wylde-ollama` first"
        );
    };
    let child = spawn_rust_binary(service_name::OLLAMA, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned ollama impl=rust pid={}", pid);
    record_spawn(service_name::OLLAMA, pid, ImplLang::Rust.as_str());
    set_service_proc(service_name::OLLAMA, child);
    Ok(())
}

pub async fn stop_ollama() -> Result<()> {
    stop_service(service_name::OLLAMA, Duration::from_secs(10)).await
}

// ── Upstream Ollama daemon (external, user-managed) ──────────────────────
//
// `wylde-ollama` (above) is the Wylde *wrapper* — a pipe proxy that talks
// to the third-party Ollama daemon at 127.0.0.1:11434. The wrapper never
// managed that daemon's lifecycle: per docs/wylde-ollama-design.md Ollama
// is a user-installed dependency that "may start lazily". That left the
// GUI's "Start wylde-ollama" stub button impotent whenever the wrapper
// pipe was up but the Ollama daemon itself was down — `service.start` saw
// the wrapper alive and no-op'd, so the panel's required-service stub
// never cleared ("clicking does nothing"). The functions below let
// `service.start wylde-ollama` actually start the upstream daemon.
//
// Unlike the daemon-managed wrappers, this process is NOT supervised: no
// spawn record, no kill_on_drop. It must OUTLIVE the Wylde stack so that
// bouncing/stopping the wrapper never tears down the user's running
// models — exactly how the daemon would behave if the user had launched
// `ollama serve` themselves.

/// Locate the `ollama` executable. Resolution order:
///   1. `WYLDE_OLLAMA_SERVE_BIN` override (must point at an existing file).
///   2. `ollama[.exe]` on `PATH`.
///   3. Default Windows install: `%LOCALAPPDATA%\Programs\Ollama\ollama.exe`.
///
/// Reads env + filesystem (no process spawn); delegates the decision to
/// the pure [`locate_ollama_binary_in`] so it is unit-testable without
/// mutating process env.
pub fn locate_ollama_binary() -> Option<PathBuf> {
    let default_dir: Option<PathBuf> = {
        #[cfg(windows)]
        {
            std::env::var_os("LOCALAPPDATA")
                .map(|l| PathBuf::from(l).join("Programs").join("Ollama"))
        }
        #[cfg(not(windows))]
        {
            None
        }
    };
    locate_ollama_binary_in(
        std::env::var_os("WYLDE_OLLAMA_SERVE_BIN"),
        std::env::var_os("PATH"),
        default_dir,
    )
}

/// Pure resolver: first an explicit `override_bin` file, then `ollama` (with
/// the host exe suffix) on any `path_var` entry, then the same exe inside
/// `default_install_dir`. Returns the first existing file.
fn locate_ollama_binary_in(
    override_bin: Option<std::ffi::OsString>,
    path_var: Option<std::ffi::OsString>,
    default_install_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(over) = override_bin {
        let p = PathBuf::from(over);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = format!("ollama{}", std::env::consts::EXE_SUFFIX);
    if let Some(paths) = path_var {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(&exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(dir) = default_install_dir {
        let cand = dir.join(&exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Spawn `ollama serve` as a detached, independent process and return the
/// binary that was launched. Best-effort: the child handle is dropped
/// WITHOUT `kill_on_drop`, so the daemon survives this process — it is the
/// user's external service, not a Wylde-supervised one.
///
/// `Err` distinguishes "not installed" (the message carries the
/// `ollama_not_installed` marker the caller maps to a stable code) from a
/// real spawn failure.
pub fn spawn_ollama_serve() -> Result<PathBuf> {
    let Some(bin) = locate_ollama_binary() else {
        anyhow::bail!(
            "ollama_not_installed: could not find the `ollama` executable (checked \
             WYLDE_OLLAMA_SERVE_BIN, PATH, and the default install location) — \
             install Ollama from https://ollama.com"
        );
    };
    // Deliberately std::process::Command, not the tokio Command used for
    // wrapper services: we want a fully detached child with NO kill_on_drop
    // so dropping the handle here leaves `ollama serve` running.
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW:
        // independent of Wylde, its own process group (a Ctrl-Break aimed
        // at the stack never reaches it), and no console window flash.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn `{} serve`", bin.display()))?;
    // Detach: a std Child's Drop does NOT signal the process, so the
    // daemon keeps running after we drop our handle.
    drop(child);
    Ok(bin)
}

// ── Tree-sitter sidecar ─────────────────────────────────────────────────
//
// Greenfield Rust — there is no Python predecessor for `wylde-treesitter`.
// Default impl is rust; the strangler-fig env var is accepted for shape
// consistency but the python branch only warns (no module to spawn). Same
// shape as `start_ollama`. See `docs/plans/treesitter-sidecar.md`.
pub async fn start_treesitter() -> Result<()> {
    if is_service_alive(service_name::TREESITTER) {
        let pid = manifest_pid(service_name::TREESITTER)
            .or_else(|| service_pid(service_name::TREESITTER))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::TREESITTER,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::TREESITTER, ImplLang::Rust.as_str());
        tracing::info!("treesitter: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    // wylde-treesitter is greenfield Rust. The strangler-fig env var is
    // accepted for shape consistency but Python isn't a valid impl.
    let lang = impl_for(service_name::TREESITTER);
    if lang == ImplLang::Python {
        tracing::warn!(
            "treesitter: WYLDE_WYLDE_TREESITTER_IMPL=python but wylde-treesitter is \
             greenfield Rust (no Python predecessor); proceeding with rust binary"
        );
    }
    let Some(bin) = rust_binary_path(service_name::TREESITTER) else {
        anyhow::bail!(
            "treesitter: rust binary not found (checked WYLDE_WYLDE_TREESITTER_BIN, \
             rust/bin/wylde-treesitter.exe, rust/target/release/wylde-treesitter.exe, \
             rust/target/debug/wylde-treesitter.exe) — build with `cargo build \
             --release -p wylde-treesitter` first"
        );
    };
    let child = spawn_rust_binary(service_name::TREESITTER, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned treesitter impl=rust pid={}", pid);
    record_spawn(service_name::TREESITTER, pid, ImplLang::Rust.as_str());
    set_service_proc(service_name::TREESITTER, child);
    Ok(())
}

pub async fn stop_treesitter() -> Result<()> {
    stop_service(service_name::TREESITTER, Duration::from_secs(10)).await
}

// ── wylde-workspaces ────────────────────────────────────────────────────
//
// Greenfield Rust — there is no Python predecessor for `wylde-workspaces`
// (Thought Bubble System Phase 0). The strangler-fig env var pattern is
// kept for shape consistency (`WYLDE_WYLDE_WORKSPACES_IMPL`) but the python
// branch only warns — there is no module to spawn. Same shape as
// `start_ollama` / `start_treesitter`. Spawned LAST in the boot sequence
// (see `daemon.rs`): it consumes `wylde-ollama` (embedder),
// `wylde-treesitter` (chunk/extract), and Memgraph (Bolt graph writes), so
// those must be up first. A missing binary leaves it down with a loud build
// hint — every consumer degrades gracefully when it's absent (Slice 0d), so
// a failed spawn is non-fatal to the rest of the stack.
pub async fn start_workspaces() -> Result<()> {
    if is_service_alive(service_name::WORKSPACES) {
        let pid = manifest_pid(service_name::WORKSPACES)
            .or_else(|| service_pid(service_name::WORKSPACES))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::WORKSPACES,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::WORKSPACES, ImplLang::Rust.as_str());
        tracing::info!("workspaces: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    // wylde-workspaces is greenfield Rust. The strangler-fig env var is
    // accepted for shape consistency but Python isn't a valid impl.
    let lang = impl_for(service_name::WORKSPACES);
    if lang == ImplLang::Python {
        tracing::warn!(
            "workspaces: WYLDE_WYLDE_WORKSPACES_IMPL=python but wylde-workspaces is \
             greenfield Rust (no Python predecessor); proceeding with rust binary"
        );
    }
    let Some(bin) = rust_binary_path(service_name::WORKSPACES) else {
        tracing::warn!(
            "workspaces: no rust binary found (checked WYLDE_WYLDE_WORKSPACES_BIN, \
             rust/bin/wylde-workspaces.exe, rust/target/release/wylde-workspaces.exe, \
             rust/target/debug/wylde-workspaces.exe); workspaces will not start — \
             consumers degrade gracefully, so the rest of the stack is unaffected. \
             Build with `cargo build --release -p wylde-workspaces`"
        );
        return Ok(());
    };
    let child = spawn_rust_binary(service_name::WORKSPACES, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned wylde-workspaces impl=rust pid={}", pid);
    record_spawn(service_name::WORKSPACES, pid, ImplLang::Rust.as_str());
    set_service_proc(service_name::WORKSPACES, child);
    Ok(())
}

pub async fn stop_workspaces() -> Result<()> {
    stop_service(service_name::WORKSPACES, Duration::from_secs(10)).await
}

// ── wylde-n8n ───────────────────────────────────────────────────────────
//
// Taxonomy reorg TX S3 — greenfield Rust (the Python `N8N/client.py` +
// tools were in-process harness code, never a daemon, so there is no
// Python service to strangle). The strangler-fig env var pattern is kept
// for shape consistency (`WYLDE_WYLDE_N8N_IMPL`) but the python branch
// only warns. This start supervises the Wylde-side pipe service ONLY —
// the n8n daemon itself is external and user-managed; `wylde-n8n`
// degrades every call to a structured error envelope while it's down.
// Optional/non-fatal by contract: a missing binary leaves the service
// dark with a loud build hint and core boots fine (the
// `wylde-workspaces` precedent — every consumer fail-softs).
pub async fn start_n8n() -> Result<()> {
    if is_service_alive(service_name::N8N) {
        let pid = manifest_pid(service_name::N8N)
            .or_else(|| service_pid(service_name::N8N))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::N8N,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::N8N, ImplLang::Rust.as_str());
        tracing::info!("n8n: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    // wylde-n8n is greenfield Rust. The strangler-fig env var is
    // accepted for shape consistency but Python isn't a valid impl.
    let lang = impl_for(service_name::N8N);
    if lang == ImplLang::Python {
        tracing::warn!(
            "n8n: WYLDE_WYLDE_N8N_IMPL=python but wylde-n8n is greenfield Rust \
             (the Python N8N client was in-process, not a service); proceeding \
             with rust binary"
        );
    }
    let Some(bin) = rust_binary_path(service_name::N8N) else {
        tracing::warn!(
            "n8n: no rust binary found (checked WYLDE_WYLDE_N8N_BIN, \
             rust/bin/wylde-n8n.exe, rust/target/release/wylde-n8n.exe, \
             rust/target/debug/wylde-n8n.exe); wylde-n8n will not start — the \
             service is optional, so the rest of the stack is unaffected. \
             Build with `cargo build --release -p wylde-n8n`"
        );
        return Ok(());
    };
    let child = spawn_rust_binary(service_name::N8N, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned wylde-n8n impl=rust pid={}", pid);
    record_spawn(service_name::N8N, pid, ImplLang::Rust.as_str());
    set_service_proc(service_name::N8N, child);
    Ok(())
}

pub async fn stop_n8n() -> Result<()> {
    stop_service(service_name::N8N, Duration::from_secs(10)).await
}

// ── wylde-harness ─────────────────────────────────────────────────────
//
// Phase 5 of the Rust migration — the consolidated harness crate.
// the Wylde user's standing instruction (2026-05-24): the harness is ONE
// logical thing — one crate, one binary, one pipe — with submodules
// for the distinct concerns (turn, tooling, memory, …).
//
// Slice 5.D (2026-05-25) flipped the strangler-fig default from
// PYTHON to RUST after byte-level parity coverage landed for the
// salvage parser, `_call_hash`, and `_find_balanced_braces` — the
// pure functions whose port fidelity is load-bearing for the
// dispatch loop (`rust/tests/parity/tests/harness_turn.rs`, 25
// cases, all green). Set `WYLDE_WYLDE_HARNESS_IMPL=python` to
// revert to the in-process Python driver during the rollback window.
//
// The Python harness (and its in-process `_chat.py` strangler
// driver) was deleted in the full-Rust cutover R6 — the Rust binary
// is the only impl, and a missing binary leaves chat.* down until
// it is built.
pub async fn start_harness() -> Result<()> {
    if is_service_alive(service_name::HARNESS) {
        let pid = manifest_pid(service_name::HARNESS)
            .or_else(|| service_pid(service_name::HARNESS))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::HARNESS,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(
            service_name::HARNESS,
            impl_for_with_default(service_name::HARNESS, ImplLang::Rust).as_str(),
        );
        tracing::info!("wylde-harness: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    let lang = impl_for_with_default(service_name::HARNESS, ImplLang::Rust);
    if lang == ImplLang::Python {
        // The Python harness was deleted in the full-Rust cutover R6 —
        // there is nothing to defer to. Warn and proceed rust-only.
        tracing::warn!(
            "wylde-harness: WYLDE_WYLDE_HARNESS_IMPL=python requested but the \
             Python harness was removed (full-Rust cutover); proceeding rust-only"
        );
    }
    let Some(bin) = rust_binary_path(service_name::HARNESS) else {
        // No binary built. The Python fallback is gone, so chat.* is
        // simply down until the binary exists — warn with the build
        // command and keep the boot sequence going (non-fatal, same
        // shape as workspaces).
        tracing::warn!(
            "wylde-harness: no rust binary found (checked WYLDE_WYLDE_HARNESS_BIN, \
             rust/bin/, rust/target/release/, rust/target/debug/); the harness will \
             not start — the Python harness was removed, so build with \
             `cargo build --release -p wylde-harness`"
        );
        return Ok(());
    };
    let child = spawn_rust_binary(service_name::HARNESS, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned wylde-harness impl=rust pid={}", pid);
    record_spawn(service_name::HARNESS, pid, ImplLang::Rust.as_str());
    set_service_proc(service_name::HARNESS, child);
    Ok(())
}

pub async fn stop_harness() -> Result<()> {
    stop_service(service_name::HARNESS, Duration::from_secs(10)).await
}

// ── wylde-vpn ─────────────────────────────────────────────────────────
//
// Phase 2 of the Rust migration. Phase 2.E (2026-05-24) flipped the
// strangler-fig default from Python to Rust after Gateway's link
// routes were cut over to the action-style pipe surface. The Python
// `VPN/run.py` Flask service was DELETED 2026-06-02 once the shared
// pipe server gained an HTTP route-table adapter and `wylde-vpn` wired
// its `GET /api/link/*` routes onto it (route-table parity) — so this
// is now rust-only with no Python fallback. The `WYLDE_WYLDE_VPN_IMPL`
// selector no longer has a `python` target.

pub async fn start_vpn() -> Result<()> {
    if is_service_alive(service_name::VPN) {
        let pid = manifest_pid(service_name::VPN)
            .or_else(|| service_pid(service_name::VPN))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::VPN,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::VPN, ImplLang::Rust.as_str());
        tracing::info!("wylde-vpn: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    // Rust-only: the Python VPN tree was deleted, so there is no
    // fallback impl. A missing binary means VPN simply doesn't start
    // (dashboard paints it down) — warn loudly with the build command
    // rather than spawning a Python service that no longer exists.
    let Some(bin) = rust_binary_path(service_name::VPN) else {
        tracing::warn!(
            "wylde-vpn: no rust binary found (checked WYLDE_WYLDE_VPN_BIN, rust/bin/, \
             rust/target/release/, rust/target/debug/); VPN will not start — the Python \
             VPN service was removed, so build with `cargo build --release -p wylde-vpn`"
        );
        return Ok(());
    };
    let child = spawn_rust_binary(service_name::VPN, &bin)?;
    let pid = child.id().unwrap_or(0);
    tracing::info!("daemon: spawned wylde-vpn impl=rust pid={}", pid);
    record_spawn(service_name::VPN, pid, ImplLang::Rust.as_str());
    set_service_proc(service_name::VPN, child);
    Ok(())
}

pub async fn stop_vpn() -> Result<()> {
    stop_service(service_name::VPN, Duration::from_secs(10)).await
}

// ── Memory scheduler ──────────────────────────────────────────────────

/// The memory scheduler became a tokio task inside the Rust
/// wylde-harness in the full-Rust cutover slice R2b
/// (`wylde_harness::memory::scheduler`, started from
/// `service::install`, gated on `WYLDE_HARNESS_SCHEDULER`, default
/// on). The daemon therefore has no scheduler of its own to spawn —
/// this start hook just records that fact in the log so the boot
/// sequence reads complete.
pub async fn start_memory_scheduler() -> Result<()> {
    tracing::info!(
        "memory_scheduler: in-process in the Rust wylde-harness since slice R2b \
         (WYLDE_HARNESS_SCHEDULER gates it there); nothing for the daemon to spawn"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env(var: &str) {
        std::env::remove_var(var);
    }

    #[test]
    fn impl_for_defaults_to_rust() {
        clear_env("WYLDE_WYLDE_TEST_IMPL");
        assert_eq!(impl_for("wylde-test"), ImplLang::Rust);
    }

    // Regression: the Neo4j supervisor MUST spawn with absolute paths. When
    // `WYLDE_ROOT` is unset (the production launcher never exports it),
    // `wylde_root()` is the relative `"."`; combined with `current_dir` set
    // to the neo4j subdir, a relative bat/JAVA_HOME/NEO4J_HOME double-nests
    // and `cmd` dies with "The system cannot find the path specified."
    // (Neo4j never boots, Bolt :7687 stays closed). `memgraph_root_abs`
    // guarantees these are absolute regardless of CWD / WYLDE_ROOT.
    #[test]
    fn memgraph_dirs_are_absolute() {
        assert!(
            memgraph_root_abs().is_absolute(),
            "memgraph root must be absolute, got {:?}",
            memgraph_root_abs()
        );
        assert!(memgraph_neo4j_dir().is_absolute());
        assert!(memgraph_jdk_dir().is_absolute());
    }

    #[test]
    fn impl_for_reads_rust() {
        std::env::set_var("WYLDE_WYLDE_RUSTSVC_IMPL", "rust");
        assert_eq!(impl_for("wylde-rustsvc"), ImplLang::Rust);
        clear_env("WYLDE_WYLDE_RUSTSVC_IMPL");
    }

    #[test]
    fn impl_for_case_insensitive() {
        std::env::set_var("WYLDE_WYLDE_CASESVC_IMPL", "RUST");
        assert_eq!(impl_for("wylde-casesvc"), ImplLang::Rust);
        clear_env("WYLDE_WYLDE_CASESVC_IMPL");
    }

    #[test]
    fn impl_for_unrecognised_falls_back_to_rust() {
        std::env::set_var("WYLDE_WYLDE_TYPOSVC_IMPL", "go");
        assert_eq!(impl_for("wylde-typosvc"), ImplLang::Rust);
        clear_env("WYLDE_WYLDE_TYPOSVC_IMPL");
    }

    #[test]
    fn rust_binary_path_honours_override() {
        // Point at an existing file (this source file) so the resolver
        // returns it. Negative case: point at a missing path → None.
        let me = std::env::current_exe().unwrap();
        std::env::set_var("WYLDE_WYLDE_OVERRIDESVC_BIN", &me);
        assert_eq!(rust_binary_path("wylde-overridesvc"), Some(me.clone()));

        std::env::set_var("WYLDE_WYLDE_OVERRIDESVC_BIN", "/no/such/path/here");
        assert_eq!(rust_binary_path("wylde-overridesvc"), None);
        clear_env("WYLDE_WYLDE_OVERRIDESVC_BIN");
    }

    #[test]
    fn rust_binary_path_strips_wylde_prefix() {
        // Without env override and without a matching file, we get None.
        // What we *do* check is that the env-var name uses the stripped
        // form (e.g. "wylde-foo" → WYLDE_WYLDE_FOO_BIN).
        clear_env("WYLDE_WYLDE_NONEXISTENT_BIN");
        assert_eq!(rust_binary_path("wylde-nonexistent"), None);
    }

    // ── Upstream Ollama binary discovery (pure resolver) ──────────────

    #[test]
    fn locate_ollama_override_wins_when_file_exists() {
        // An existing file via the explicit override is returned verbatim,
        // ahead of PATH and the default install dir.
        let me = std::env::current_exe().unwrap();
        let got = locate_ollama_binary_in(Some(me.clone().into_os_string()), None, None);
        assert_eq!(got, Some(me));
    }

    #[test]
    fn locate_ollama_override_missing_file_is_skipped() {
        let got = locate_ollama_binary_in(
            Some(std::ffi::OsString::from("/no/such/ollama/here")),
            None,
            None,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn locate_ollama_finds_exe_on_path() {
        // Drop a real `ollama<EXE_SUFFIX>` file in a temp dir, hand that dir
        // as the only PATH entry, and assert the resolver finds it.
        let dir = tempfile::tempdir().unwrap();
        let exe = format!("ollama{}", std::env::consts::EXE_SUFFIX);
        let bin = dir.path().join(&exe);
        std::fs::write(&bin, b"#!stub").unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let got = locate_ollama_binary_in(None, Some(path_var), None);
        assert_eq!(got, Some(bin));
    }

    #[test]
    fn locate_ollama_falls_back_to_default_install_dir() {
        let dir = tempfile::tempdir().unwrap();
        let exe = format!("ollama{}", std::env::consts::EXE_SUFFIX);
        let bin = dir.path().join(&exe);
        std::fs::write(&bin, b"#!stub").unwrap();
        // No override, empty PATH → the default install dir is consulted.
        let got = locate_ollama_binary_in(None, None, Some(dir.path().to_path_buf()));
        assert_eq!(got, Some(bin));
    }

    #[test]
    fn locate_ollama_none_when_nothing_matches() {
        let empty = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([empty.path()]).unwrap();
        let got = locate_ollama_binary_in(None, Some(path_var), Some(empty.path().to_path_buf()));
        assert_eq!(got, None);
    }

    // ── Strangler-fig start table ──────────────────────────────────────

    #[test]
    fn strangler_table_covers_the_five_dispatched_services() {
        // The table holds exactly the five near-identical two-impl
        // services. The four unique services (memgraph, ollama, harness,
        // vpn) are deliberately NOT here.
        let names: Vec<&str> = STRANGLER_SERVICES.iter().map(|d| d.name).collect();
        assert_eq!(names.len(), 5);
        for expected in [
            service_name::DEVICE_GATE,
            service_name::VRAM_BROKER,
            service_name::EXTENSION_BRIDGE,
            service_name::GATEWAY,
            service_name::VOICE,
        ] {
            assert!(
                names.contains(&expected),
                "strangler table missing {expected}"
            );
        }
        // Unique services must NOT have leaked into the table.
        for unique in [
            service_name::MEMGRAPH,
            service_name::OLLAMA,
            service_name::HARNESS,
            service_name::VPN,
            service_name::TREESITTER,
            service_name::WORKSPACES,
            service_name::N8N,
        ] {
            assert!(
                !names.contains(&unique),
                "{unique} must stay hand-written, not in the table"
            );
        }
    }

    #[tokio::test]
    async fn start_workspaces_without_binary_is_non_fatal() {
        // wylde-workspaces is greenfield Rust with no Python fallback. When
        // no binary resolves (the env in this unit test), start must NOT
        // bail — a missing binary leaves the service down but every consumer
        // degrades gracefully (Slice 0d), so boot must continue. Point the
        // BIN override at a missing path so the resolver returns None
        // deterministically regardless of any built target on disk.
        std::env::set_var("WYLDE_WYLDE_WORKSPACES_BIN", "/no/such/workspaces/binary");
        let result = start_workspaces().await;
        std::env::remove_var("WYLDE_WYLDE_WORKSPACES_BIN");
        assert!(
            result.is_ok(),
            "start_workspaces with no binary must be a non-fatal no-op, got {result:?}"
        );
    }

    #[tokio::test]
    async fn start_n8n_without_binary_is_non_fatal() {
        // wylde-n8n is OPTIONAL by contract — core must boot fine when the
        // binary is missing (or the external n8n daemon is down). Point the
        // BIN override at a missing path so the resolver returns None
        // deterministically regardless of any built target on disk.
        std::env::set_var("WYLDE_WYLDE_N8N_BIN", "/no/such/n8n/binary");
        let result = start_n8n().await;
        std::env::remove_var("WYLDE_WYLDE_N8N_BIN");
        assert!(
            result.is_ok(),
            "start_n8n with no binary must be a non-fatal no-op, got {result:?}"
        );
    }

    #[test]
    fn strangler_defs_carry_expected_default() {
        // Default impl per row. ALL five are rust-only — device_gate,
        // vram_broker, gateway on 2026-06-02, voice in the Phase 11.E
        // cutover, and extension_bridge in the full-Rust cutover
        // (2026-06-09, dogfood gate waived by Aaron). The
        // `python_module` field itself went with the Python runtime
        // tree in slice R6.
        let cases = [
            (service_name::DEVICE_GATE, ImplLang::Rust),
            (service_name::VRAM_BROKER, ImplLang::Rust),
            (service_name::EXTENSION_BRIDGE, ImplLang::Rust),
            (service_name::GATEWAY, ImplLang::Rust),
            (service_name::VOICE, ImplLang::Rust),
        ];
        for (name, default) in cases {
            let def = strangler_def(name);
            assert_eq!(def.default_impl, default, "default mismatch for {name}");
            assert!(
                !def.missing_binary_warn.is_empty(),
                "{name} missing its no-binary warning"
            );
        }
    }

    #[test]
    fn strangler_def_resolves_each_name() {
        // `strangler_def` must find every table entry by name.
        for def in STRANGLER_SERVICES {
            assert_eq!(strangler_def(def.name).name, def.name);
        }
    }

    // ── Discovered out-of-tree sibling supervision ─────────────────────

    #[test]
    fn sibling_binary_path_honours_override() {
        // The WYLDE_<NAME>_BIN override (dev staging) wins and must point at
        // an existing file; a missing override path resolves to None.
        let me = std::env::current_exe().unwrap();
        std::env::set_var("WYLDE_WYLDE_IMAGES_BIN", &me);
        let folder = std::path::Path::new("Services/wylde-images");
        assert_eq!(sibling_binary_path(folder, "wylde-images"), Some(me));
        std::env::set_var("WYLDE_WYLDE_IMAGES_BIN", "/no/such/sibling/bin");
        assert_eq!(sibling_binary_path(folder, "wylde-images"), None);
        std::env::remove_var("WYLDE_WYLDE_IMAGES_BIN");
    }

    #[test]
    fn sibling_binary_path_finds_beside_manifest() {
        // The release drop location: Services/<name>/<bin>.exe (next to the
        // manifest), where `cargo xtask build-all` stages the artifact.
        std::env::remove_var("WYLDE_WYLDE_GALLERY_BIN");
        let dir = tempfile::tempdir().unwrap();
        let bin_name = format!("wylde-gallery{}", if cfg!(windows) { ".exe" } else { "" });
        let bin = dir.path().join(&bin_name);
        std::fs::write(&bin, b"#!stub").unwrap();
        assert_eq!(sibling_binary_path(dir.path(), "wylde-gallery"), Some(bin));
    }

    #[tokio::test]
    async fn start_discovered_without_binary_is_non_fatal() {
        // A discovered sibling with no resolvable binary must NOT bail —
        // the sibling stays down but core boot continues. Point the BIN
        // override at a missing path so resolution is deterministically None
        // regardless of any artifact on disk.
        std::env::set_var("WYLDE_WYLDE_PHANTOM_BIN", "/no/such/phantom/binary");
        let svc = crate::registry::DiscoveredService {
            name: "wylde-phantom".to_string(),
            folder: std::path::PathBuf::from("Services/wylde-phantom"),
            enabled: true,
            min_core: None,
        };
        let result = start_discovered(&svc).await;
        std::env::remove_var("WYLDE_WYLDE_PHANTOM_BIN");
        assert!(
            result.is_ok(),
            "start_discovered with no binary must be a non-fatal no-op, got {result:?}"
        );
    }

    #[tokio::test]
    async fn start_discovered_refuses_incompatible_min_core() {
        // A sibling that needs a newer Core than is running is REFUSED before any
        // spawn — non-fatal (Ok), no child forked. The BIN override points at a
        // real-but-unexecutable file: had the floor check NOT fired first,
        // start_discovered would reach the spawn and return Err trying to exec it.
        // So `Ok` proves the floor short-circuited ahead of the spawn. (The
        // comparison logic itself is proven in
        // registry::tests::check_core_floor_semantics.)
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("not-an-exe.txt");
        std::fs::write(&fake_bin, b"not executable").unwrap();
        std::env::set_var("WYLDE_WYLDE_INCOMPAT_BIN", &fake_bin);
        let svc = crate::registry::DiscoveredService {
            name: "wylde-incompat".to_string(),
            folder: dir.path().to_path_buf(),
            enabled: true,
            min_core: Some("99.0.0".to_string()), // far above any real Core
        };
        let result = start_discovered(&svc).await;
        std::env::remove_var("WYLDE_WYLDE_INCOMPAT_BIN");
        assert!(
            result.is_ok(),
            "an incompatible sibling must be refused non-fatally BEFORE spawn, got {result:?}"
        );
    }
}
