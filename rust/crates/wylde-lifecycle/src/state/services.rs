//! Seven daemon-managed service start/stop pairs.
//!
//! Rust port of `Core/Lifecycle/daemon_state/_services.py`. Memgraph,
//! Voice, device_gate, vram_broker, extension_bridge, gateway,
//! memory_scheduler. Each `start_<service>` boots the service as a
//! subprocess and records the spawn so orphan-detection knows about
//! it. Each `stop_<service>` sends the OS-appropriate graceful signal,
//! waits for exit, and force-kills on timeout.
//!
//! ## Strangler-fig switch
//!
//! Services with both impls (extension_bridge, voice) are dispatched
//! through [`impl_for`]: `WYLDE_<SERVICE>_IMPL=rust` picks a sibling Rust
//! binary resolved by [`rust_binary_path`], and a missing or unparseable
//! env var (or a missing Rust binary) falls back to the Python module
//! with a warning, so a mis-set deployment can never silently lose the
//! service. vram_broker, device_gate, and gateway were collapsed to
//! Rust-only on 2026-06-02 — their Python packages were deleted, so they
//! have no fallback (a missing binary leaves them down). This is the SAME
//! dispatch the Python daemon uses — we port it verbatim so behaviour
//! stays consistent regardless of which daemon is running.
//!
//! Memory scheduler note: the Python daemon hosts the scheduler
//! in-process (it's a Python thread; no separate binary). The Rust
//! daemon can't host it in-process — there's no Rust scheduler — so
//! [`start_memory_scheduler`] currently logs a one-time advisory and
//! returns. the Wylde user's launcher script can opt back to the Python
//! daemon if the scheduler is required for a given session.
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
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

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

/// Read `WYLDE_<SERVICE>_IMPL` for `service`; default Python.
///
/// The service name `wylde-vram-broker` maps to env var
/// `WYLDE_WYLDE_VRAM_BROKER_IMPL` — dashes become underscores,
/// everything uppercased. Unrecognised values log a warning and fall
/// back to `python` so a typo can't take a service offline.
pub fn impl_for(service: &str) -> ImplLang {
    impl_for_with_default(service, ImplLang::Python)
}

/// Same as [`impl_for`] but with a per-service default for when the env
/// var is unset or unrecognised. Used by services whose default has
/// been flipped to Rust — VPN ships `default = Rust` after Phase 2.E
/// even though the Python implementation is still on disk as a
/// rollback path.
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

fn python_executable() -> PathBuf {
    // The torn-venv investigation lesson (see memory:
    // `wylde_py3_resolves_to_python_314`): never use `py -3`. The
    // canonical interpreter for the project is the .venv's
    // python.exe. Honour an explicit override (`WYLDE_PYTHON`) when
    // set, otherwise fall back to the venv path under WYLDE_ROOT, and
    // finally to a plain `python` lookup so dev shells still work.
    if let Ok(p) = std::env::var("WYLDE_PYTHON") {
        return PathBuf::from(p);
    }
    let venv = wylde_root()
        .join(".venv")
        .join("Scripts")
        .join("python.exe");
    if venv.exists() {
        return venv;
    }
    PathBuf::from("python")
}

fn namespace_pythonpath() -> String {
    // Children spawned with `cwd = WYLDE_ROOT` need
    // `parent-of-WYLDE_ROOT` on `PYTHONPATH` so `from Wylde.X import Y`
    // resolves. Mirrors `_services.py`'s overlay.
    let mut p = wylde_root();
    p.pop();
    let mut path = p.to_string_lossy().into_owned();
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        if !existing.is_empty() {
            path.push(';');
            path.push_str(&existing);
        }
    }
    path
}

/// Spawn helper for `python -m <module>`. Returns the live Child and
/// the pid we should record. Always uses
/// `CREATE_NEW_PROCESS_GROUP` so we can later send `CTRL_BREAK_EVENT`
/// without taking the parent down with us.
fn spawn_python_module(module: &str, service_name: &str) -> Result<Child> {
    let py = python_executable();
    tracing::info!(
        "daemon: spawning {} via python -m {} (interpreter={})",
        service_name,
        module,
        py.display()
    );

    let mut cmd = Command::new(&py);
    cmd.arg("-m")
        .arg(module)
        .current_dir(wylde_root())
        .env("WYLDE_SERVICE_NAME", service_name)
        .env("WYLDE_ROOT", wylde_root())
        .env("PYTHONPATH", namespace_pythonpath())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    apply_kill_on_drop(&mut cmd);

    cmd.spawn()
        .with_context(|| format!("spawn python -m {module} for {service_name}"))
}

/// Spawn helper for a Rust service binary. Same Stdio + creation
/// flags as the Python branch so signal handling stays uniform.
fn spawn_rust_binary(service_name: &str, rust_bin: &Path) -> Result<Child> {
    tracing::info!(
        "daemon: spawning {} via rust binary {}",
        service_name,
        rust_bin.display()
    );
    let mut cmd = Command::new(rust_bin);
    cmd.current_dir(wylde_root())
        .env("WYLDE_SERVICE_NAME", service_name)
        .env("WYLDE_ROOT", wylde_root())
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
/// use 10s; Memgraph and the trainer-worker use 15s (Neo4j / CUDA
/// teardown). Keeping the wrappers (rather than calling this directly
/// from `control.rs`) preserves the `pub async fn stop_<service>()`
/// public API the daemon dispatches by name.
async fn stop_service(name: &str, grace: Duration) -> Result<()> {
    forget_spawn(name);
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
// → already-alive guard → dispatch → record + track. The only things
// that vary are the service name, the Python module, the per-service
// default impl, and the "no binary found" warning text — so they live in
// a table and share one generic [`start_strangler`]. Two of them
// (extension_bridge, voice) keep a Python fallback (`python_module:
// Some(..)`): a missing Rust binary falls back to `python -m <module>`.
// The other three (device_gate, vram_broker, gateway) were collapsed to
// Rust-only on 2026-06-02 (`python_module: None`) when their Python
// packages were deleted — a missing binary leaves them down, with no
// fallback. The unique services (memgraph, ollama, harness, trainer,
// trainer_worker, vpn) stay hand-written below because their control
// flow genuinely diverges (always-python, hard-fail,
// early-return-no-spawn, script-not-module, conditional-on-sibling-impl).

/// One row of the strangler-fig start table.
struct StranglerService {
    /// Canonical service name (e.g. `service_name::VOICE`).
    name: &'static str,
    /// Python fallback module, run as `python -m <python_module>`, or
    /// `None` for services collapsed to Rust-only. A `None` row has no
    /// Python impl on disk, so a missing Rust binary means the service
    /// simply does not start (no fallback) — the VPN pattern.
    python_module: Option<&'static str>,
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
        // is no Python fallback — `python_module: None` means a missing
        // binary leaves the service down rather than spawning a module
        // that no longer exists. `WYLDE_WYLDE_DEVICE_GATE_IMPL` no longer
        // has a `python` target.
        name: service_name::DEVICE_GATE,
        python_module: None,
        default_impl: ImplLang::Rust,
        missing_binary_warn:
            "device_gate: no rust binary found; device_gate will not start — the \
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
        // Python fallback (`python_module: None`).
        name: service_name::VRAM_BROKER,
        python_module: None,
        default_impl: ImplLang::Rust,
        missing_binary_warn:
            "vram_broker: no rust binary found; vram_broker will not start — the \
             Python Core/resource_monitor package was removed, so build with \
             `cargo build --release -p wylde-vram-broker`",
    },
    StranglerService {
        name: service_name::EXTENSION_BRIDGE,
        python_module: Some("Extensions.extension_bridge.run"),
        default_impl: ImplLang::Python,
        missing_binary_warn:
            "extension_bridge: WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL=rust but no \
             binary found; falling back to python",
    },
    StranglerService {
        // Collapsed to Rust-only (2026-06-02): the in-tree Python
        // `Gateway` package was DELETED; the Rust `wylde-gateway` (axum)
        // — a superset of the Python routes — is the canonical
        // ingress/egress. There is no Python fallback (`python_module:
        // None`); `WYLDE_WYLDE_GATEWAY_IMPL` no longer has a `python`
        // target.
        name: service_name::GATEWAY,
        python_module: None,
        default_impl: ImplLang::Rust,
        missing_binary_warn:
            "gateway: no rust binary found; gateway will not start — the Python \
             Gateway package was removed, so build with `cargo build --release \
             -p wylde-gateway`",
    },
    StranglerService {
        name: service_name::VOICE,
        python_module: Some("Voice.run"),
        default_impl: ImplLang::Rust,
        missing_binary_warn:
            "voice: default impl=rust but no binary found; falling back to python \
             (rollback path) — build with `cargo build --release -p wylde-voice` \
             to engage rust",
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
        let pid = manifest_pid(def.name).or_else(|| service_pid(def.name)).unwrap_or(0);
        tracing::info!("{}: already alive (manifest pid={}); skipping spawn", def.name, pid);
        return Ok(());
    }
    if nospawn_enabled() {
        // Rust-only rows (`python_module: None`) record `rust` regardless
        // of any stale `=python` override — there is no Python impl to
        // record. Two-impl rows record whichever impl would have spawned.
        let recorded = match def.python_module {
            None => ImplLang::Rust,
            Some(_) => impl_for_with_default(def.name, def.default_impl),
        };
        nospawn_record(def.name, recorded.as_str());
        tracing::info!(
            "{}: NO-SPAWN — would-have-spawned recorded; no child forked",
            def.name
        );
        return Ok(());
    }
    let want = impl_for_with_default(def.name, def.default_impl);
    let (child, impl_lang) = match def.python_module {
        // Rust-only: no Python fallback. A missing binary leaves the
        // service down (VPN pattern); an explicit `=python` override is
        // honoured only as a warning — the module was removed.
        None => {
            if want == ImplLang::Python {
                tracing::warn!(
                    "{}: WYLDE_{}_IMPL=python requested but the Python impl was \
                     removed; this service is rust-only",
                    def.name,
                    def.name.to_uppercase().replace('-', "_"),
                );
            }
            match rust_binary_path(def.name) {
                Some(bin) => (spawn_rust_binary(def.name, &bin)?, ImplLang::Rust),
                None => {
                    tracing::warn!("{}", def.missing_binary_warn);
                    return Ok(());
                }
            }
        }
        // Two-impl: Rust binary if resolvable, else fall back to the
        // Python module.
        Some(module) => match want {
            ImplLang::Rust => match rust_binary_path(def.name) {
                Some(bin) => (spawn_rust_binary(def.name, &bin)?, ImplLang::Rust),
                None => {
                    tracing::warn!("{}", def.missing_binary_warn);
                    (spawn_python_module(module, def.name)?, ImplLang::Python)
                }
            },
            ImplLang::Python => (spawn_python_module(module, def.name)?, ImplLang::Python),
        },
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

/// Boot Memgraph as a subprocess of the Lifecycle daemon.
///
/// Memgraph stays Python-only: `Core/Memgraph/run.py` spawns a Neo4j
/// JVM child, manipulates `sys.path`, and installs its own signal
/// handler. Pulling that into the daemon process would mix signal
/// handlers and pollute namespaces, so we spawn it as a subprocess
/// (same as the Python daemon does).
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
        nospawn_record(service_name::MEMGRAPH, ImplLang::Python.as_str());
        tracing::info!("memgraph: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    let child = match spawn_python_module("Core.Memgraph.run", service_name::MEMGRAPH) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("daemon: memgraph spawn failed: {:#}", e);
            return Err(e);
        }
    };
    let pid = child.id().unwrap_or(0);
    tracing::info!(
        "memgraph: spawned (pid={}) — Neo4j boot may take up to 120s",
        pid
    );
    record_spawn(service_name::MEMGRAPH, pid, ImplLang::Python.as_str());
    set_service_proc(service_name::MEMGRAPH, child);
    Ok(())
}

pub async fn stop_memgraph() -> Result<()> {
    // Memgraph holds Neo4j; give it a longer grace window than the
    // others (15s, mirroring _services.py).
    stop_service(service_name::MEMGRAPH, Duration::from_secs(15)).await
}

// ── Voice ─────────────────────────────────────────────────────────────
//
// Phase 11a (Slice 11.A — 2026-05-24): strangler-fig dispatch added.
// Slice 11.E+ (2026-05-27): default flipped python → rust now that the
// GUI-facing surface (`voice.toggle` / `voice.set_mode` / friends) is
// ported. `WYLDE_WYLDE_VOICE_IMPL=python` is still honoured as the
// rollback path during the 2-4 week strangler-fig soak; the Python
// `Voice/` tree stays on disk for that window and is then removed in
// the cleanup slice. The `default = Rust` carry mirrors `WYLDE_VPN_IMPL`
// after Phase 2.E.

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
/// Phase 4 strangler-fig: the historical Python impl
/// (`Extensions.extension_bridge.run`, importlib-based dispatcher) and
/// the Rust port (`wylde-extension-bridge`, MCP-server host) coexist;
/// impl is selected via `WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL=python|rust`.
/// Default is `python`. The Rust impl is a major contract change
/// (MCP-server host instead of in-process `importlib`); per master
/// plan §11 Q-E1 we want at least one dogfood week before flipping
/// the default. Both impls bind the SAME pipe and accept the SAME
/// `extensions.dispatch` action shape so Gateway routing is
/// unchanged — the Rust impl additionally exposes nine `ext.*`
/// actions plus the `ext.events` stream.
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
// The Python harness "impl" is the in-process driver inside
// the existing Python harness service; when no Rust binary is
// present we simply don't spawn — the Python harness pipe handles
// `chat.*` calls as it always has. The strangler-fig switch happens
// at the Python `_chat.py` action handler, not at the lifecycle
// daemon.
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
        tracing::info!(
            "wylde-harness: NO-SPAWN — would-have-spawned recorded; no child forked"
        );
        return Ok(());
    }
    let lang = impl_for_with_default(service_name::HARNESS, ImplLang::Rust);
    if lang == ImplLang::Python {
        // Python "impl" means: the in-process driver inside the
        // existing wylde-harness Python service handles chat.*. No
        // separate child to spawn from here. Returning Ok keeps the
        // daemon's start_all loop clean.
        tracing::info!(
            "wylde-harness: WYLDE_WYLDE_HARNESS_IMPL=python — chat driver \
             stays in-process on the Python harness; no daemon-managed subprocess"
        );
        return Ok(());
    }
    let Some(bin) = rust_binary_path(service_name::HARNESS) else {
        // Rust impl requested (the default after slice 5.D) but no
        // binary built. Warn loudly and fall through to "no
        // subprocess" — the Python harness's _chat.py strangler will
        // see no manifest at the pipe and keep using the Python
        // driver, so behaviour stays correct.
        tracing::warn!(
            "wylde-harness: rust impl requested (default after slice 5.D) but no \
             binary found (checked WYLDE_WYLDE_HARNESS_BIN, rust/bin/, \
             rust/target/release/, rust/target/debug/); falling back to Python \
             in-process driver — build with `cargo build --release -p wylde-harness`"
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

// ── wylde-trainer-worker ──────────────────────────────────────────────
//
// Python inference engine for the Rust `wylde-trainer`. Spawned only
// when `WYLDE_WYLDE_TRAINER_IMPL=rust` — in python (in-process) mode
// Caption stays where it is and the worker is not needed. Lives in the
// lifecycle crate because the `no_external_process_spawn_rust` lint
// pins `Command::new` here.

pub async fn start_trainer_worker() -> Result<()> {
    if is_service_alive(service_name::TRAINER_WORKER) {
        let pid = manifest_pid(service_name::TRAINER_WORKER)
            .or_else(|| service_pid(service_name::TRAINER_WORKER))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::TRAINER_WORKER,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(service_name::TRAINER_WORKER, ImplLang::Python.as_str());
        tracing::info!(
            "wylde-trainer-worker: NO-SPAWN — would-have-spawned recorded; no child forked"
        );
        return Ok(());
    }
    if impl_for(service_name::TRAINER) != ImplLang::Rust {
        tracing::info!(
            "wylde-trainer-worker: skipped (WYLDE_WYLDE_TRAINER_IMPL=python); \
             Caption stays in-process"
        );
        return Ok(());
    }
    let child = spawn_python_script(
        "Trainer/Caption/rust_worker.py",
        service_name::TRAINER_WORKER,
    )?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(
        "daemon: spawned wylde-trainer-worker impl=python pid={}",
        pid
    );
    record_spawn(
        service_name::TRAINER_WORKER,
        pid,
        ImplLang::Python.as_str(),
    );
    set_service_proc(service_name::TRAINER_WORKER, child);
    Ok(())
}

pub async fn stop_trainer_worker() -> Result<()> {
    // Allow up to 15s — releasing Florence-2 weights and CUDA caches
    // can take a few seconds on a hot run; same window the Python
    // teardown uses.
    stop_service(service_name::TRAINER_WORKER, Duration::from_secs(15)).await
}

// ── wylde-trainer ─────────────────────────────────────────────────────
//
// Phase 3 of the Rust migration. Default Python = in-process (the
// daemon does not spawn a Caption subprocess; existing in-process
// callers in `Trainer/Caption/` keep working). Flipping
// `WYLDE_WYLDE_TRAINER_IMPL=rust` spawns the Rust `wylde-trainer`
// binary fronting Florence-2 over `\\.\pipe\wylde-trainer`. The
// inference itself runs in the sibling `wylde-trainer-worker` Python
// service — see `start_trainer_worker` above.

pub async fn start_trainer() -> Result<()> {
    if is_service_alive(service_name::TRAINER) {
        let pid = manifest_pid(service_name::TRAINER)
            .or_else(|| service_pid(service_name::TRAINER))
            .unwrap_or(0);
        tracing::info!(
            "{}: already alive (manifest pid={}); skipping spawn",
            service_name::TRAINER,
            pid
        );
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(
            service_name::TRAINER,
            impl_for(service_name::TRAINER).as_str(),
        );
        tracing::info!(
            "wylde-trainer: NO-SPAWN — would-have-spawned recorded; no child forked"
        );
        return Ok(());
    }
    match impl_for(service_name::TRAINER) {
        ImplLang::Rust => {
            let Some(bin) = rust_binary_path(service_name::TRAINER) else {
                tracing::warn!(
                    "wylde-trainer: WYLDE_WYLDE_TRAINER_IMPL=rust but no binary found; \
                     falling back to in-process python (Caption stays in-process)"
                );
                return Ok(());
            };
            let child = spawn_rust_binary(service_name::TRAINER, &bin)?;
            let pid = child.id().unwrap_or(0);
            tracing::info!(
                "daemon: spawned wylde-trainer impl=rust pid={} binary={}",
                pid,
                bin.display()
            );
            record_spawn(service_name::TRAINER, pid, ImplLang::Rust.as_str());
            set_service_proc(service_name::TRAINER, child);
            Ok(())
        }
        ImplLang::Python => {
            // In-process mode — daemon does not manage Caption. Log
            // the state so an operator running `service.list` doesn't
            // wonder why the slot is empty.
            tracing::info!(
                "wylde-trainer: in-process mode (WYLDE_WYLDE_TRAINER_IMPL=python); \
                 daemon does not spawn a Caption subprocess. Existing in-process \
                 callers continue to work."
            );
            Ok(())
        }
    }
}

pub async fn stop_trainer() -> Result<()> {
    stop_service(service_name::TRAINER, Duration::from_secs(10)).await
}

/// Spawn helper for `python <script-relative-to-WYLDE_ROOT>` — the
/// script-path variant of `spawn_python_module`, for Python entry points
/// that are a path rather than a `-m module` (e.g. the trainer worker's
/// `Trainer/Caption/rust_worker.py`). VPN used to use this for
/// `VPN/run.py`, but that tree was deleted (rust-only now).
fn spawn_python_script(script_rel: &str, service_name: &str) -> Result<Child> {
    let py = python_executable();
    let script = wylde_root().join(script_rel);
    tracing::info!(
        "daemon: spawning {} via {} {} (interpreter={})",
        service_name,
        py.display(),
        script.display(),
        py.display(),
    );

    let mut cmd = Command::new(&py);
    cmd.arg(&script)
        .current_dir(wylde_root())
        .env("WYLDE_SERVICE_NAME", service_name)
        .env("WYLDE_ROOT", wylde_root())
        .env("PYTHONPATH", namespace_pythonpath())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    apply_kill_on_drop(&mut cmd);

    cmd.spawn()
        .with_context(|| format!("spawn {} {} for {}", py.display(), script.display(), service_name))
}

// ── Memory scheduler ──────────────────────────────────────────────────

/// The memory scheduler is an in-process Python thread spawned via
/// `Core.harness.memory.scheduler.MemoryScheduler.start`. The Rust
/// daemon has no in-process equivalent — there's no Rust scheduler
/// crate — so for now we log a one-time advisory. the Wylde user's launcher
/// script can pick the Python daemon for sessions where reflection +
/// curation cycles are needed.
pub async fn start_memory_scheduler() -> Result<()> {
    tracing::info!(
        "memory_scheduler: skipped — Python-only in-process subsystem; \
         set WYLDE_LIFECYCLE_IMPL=python to enable"
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
    fn impl_for_defaults_to_python() {
        clear_env("WYLDE_WYLDE_TEST_IMPL");
        assert_eq!(impl_for("wylde-test"), ImplLang::Python);
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
    fn impl_for_unrecognised_falls_back_to_python() {
        std::env::set_var("WYLDE_WYLDE_TYPOSVC_IMPL", "go");
        assert_eq!(impl_for("wylde-typosvc"), ImplLang::Python);
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

    // ── Strangler-fig start table ──────────────────────────────────────

    #[test]
    fn strangler_table_covers_the_five_dispatched_services() {
        // The table holds exactly the five near-identical two-impl
        // services. The six unique services (memgraph, ollama, harness,
        // trainer, trainer_worker, vpn) are deliberately NOT here.
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
            service_name::TRAINER,
            service_name::TRAINER_WORKER,
            service_name::VPN,
        ] {
            assert!(
                !names.contains(&unique),
                "{unique} must stay hand-written, not in the table"
            );
        }
    }

    #[test]
    fn strangler_defs_carry_expected_module_and_default() {
        // Module + default impl per row. extension_bridge and voice keep
        // a Python fallback (`Some(module)`); device_gate, vram_broker,
        // and gateway were collapsed to Rust-only on 2026-06-02 when their
        // Python packages were deleted (`None`). All five default to Rust
        // except extension_bridge (still Python pending its dogfood week).
        let cases = [
            (service_name::DEVICE_GATE, None, ImplLang::Rust),
            (service_name::VRAM_BROKER, None, ImplLang::Rust),
            (
                service_name::EXTENSION_BRIDGE,
                Some("Extensions.extension_bridge.run"),
                ImplLang::Python,
            ),
            (service_name::GATEWAY, None, ImplLang::Rust),
            (service_name::VOICE, Some("Voice.run"), ImplLang::Rust),
        ];
        for (name, module, default) in cases {
            let def = strangler_def(name);
            assert_eq!(def.python_module, module, "module mismatch for {name}");
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
}
