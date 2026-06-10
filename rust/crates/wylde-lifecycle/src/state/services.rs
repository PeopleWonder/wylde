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
//! Services with both impls (extension_bridge) are dispatched through
//! [`impl_for`]: `WYLDE_<SERVICE>_IMPL=rust` picks a sibling Rust binary
//! resolved by [`rust_binary_path`], and a missing or unparseable env var
//! (or a missing Rust binary) falls back to the Python module with a
//! warning, so a mis-set deployment can never silently lose the service.
//! vram_broker, device_gate, and gateway were collapsed to Rust-only on
//! 2026-06-02, and voice in the Phase 11.E cutover — their Python packages
//! were deleted, so they have no fallback (a missing binary leaves them
//! down). This is the SAME dispatch the Python daemon uses — we port it
//! verbatim so behaviour stays consistent regardless of which daemon is
//! running.
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
/// use 10s; Memgraph uses 15s (Neo4j teardown). Keeping the wrappers
/// (rather than calling this directly
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
// a table and share one generic [`start_strangler`]. One of them
// (extension_bridge) keeps a Python fallback (`python_module:
// Some(..)`): a missing Rust binary falls back to `python -m <module>`.
// The other four (device_gate, vram_broker, gateway, voice) were
// collapsed to Rust-only (`python_module: None`) when their Python
// packages were deleted — device_gate/vram_broker/gateway on 2026-06-02,
// voice in the Phase 11.E cutover — so a missing binary leaves them down,
// with no fallback. The unique services (memgraph, ollama, harness, vpn)
// stay hand-written below because their control flow genuinely diverges
// (always-python, hard-fail, early-return-no-spawn).

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
        python_module: None,
        default_impl: ImplLang::Rust,
        missing_binary_warn:
            "extension_bridge: no rust binary found; extension_bridge will not \
             start — the Python Extensions/extension_bridge module was removed, \
             so build with `cargo build --release -p wylde-extension-bridge`",
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
        // Collapsed to Rust-only (Phase 11.E cutover): the Python `Voice/`
        // tree was DELETED once `wylde-voice` (cpal + ort Whisper/Kokoro +
        // openWakeWord) reached parity and the live session STT/TTS paths
        // moved in-process (orchestrator calls `voice.transcribe` /
        // `voice.synthesize` directly). There is no Python fallback
        // (`python_module: None`); `WYLDE_WYLDE_VOICE_IMPL` no longer has a
        // `python` target.
        name: service_name::VOICE,
        python_module: None,
        default_impl: ImplLang::Rust,
        missing_binary_warn:
            "voice: no rust binary found; voice will not start — the Python \
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

fn memgraph_neo4j_dir() -> PathBuf {
    wylde_root()
        .join("Core")
        .join("Memgraph")
        .join("vendor")
        .join("neo4j")
}

fn memgraph_jdk_dir() -> PathBuf {
    wylde_root()
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
        let logs_dir = wylde_root().join("Core").join("Memgraph").join("logs");
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("create {}", logs_dir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_dir.join("neo4j.log"))
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
        tracing::info!(
            "memgraph: spawned Neo4j launcher (pid={pid}) — boot may take up to 120s"
        );
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
// `WYLDE_WYLDE_VOICE_IMPL=python` retired, so voice is now Rust-only
// (`python_module: None` in the table above) — same shape as
// device_gate/vram_broker/gateway/vpn.

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

    #[test]
    fn strangler_defs_carry_expected_module_and_default() {
        // Module + default impl per row. ALL five are Rust-only now —
        // device_gate, vram_broker, gateway on 2026-06-02, voice in the
        // Phase 11.E cutover, and extension_bridge in the full-Rust
        // cutover (2026-06-09, dogfood gate waived by Aaron) — so every
        // row carries `None` and defaults to Rust.
        let cases = [
            (service_name::DEVICE_GATE, None, ImplLang::Rust),
            (service_name::VRAM_BROKER, None, ImplLang::Rust),
            (service_name::EXTENSION_BRIDGE, None, ImplLang::Rust),
            (service_name::GATEWAY, None, ImplLang::Rust),
            (service_name::VOICE, None, ImplLang::Rust),
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
