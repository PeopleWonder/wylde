//! The Wylde-managed n8n engine (`wylde-n8n-engine`).
//!
//! n8n is a Node runtime Wylde does not own — launching it is the same
//! sanctioned third-party-process pattern as the Ollama and Memgraph
//! daemons, which is why it lives here and not in `wylde-n8n`: the
//! lifecycle daemon is the crate allowed to spawn external processes.
//! `wylde-n8n` (the pipe service) only attaches to what this starts.
//!
//! The engine gets a loopback-only, telemetry-off environment, its
//! database in the out-of-tree `WyldeData/n8n/` dir (the same data dir
//! `wylde-n8n` is handed as `WYLDE_N8N_DATA_DIR`), and the encryption key
//! from the Wylde-owned identity this module mints there on first launch —
//! so saved credentials decrypt across restarts and `wylde-n8n` reads the
//! owner account back from the same file.
//!
//! Optional by contract: opted out (`WYLDE_N8N_MANAGED=0`), Node or n8n not
//! installed, or a daemon already answering on the port all leave the
//! engine unspawned with one log line saying why, and core boots fine.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use wylde_shared::n8n::N8nIdentity;

use super::services::{apply_kill_on_drop, stop_service, ImplLang};
use crate::state::{
    is_service_alive, manifest_pid, nospawn_enabled, nospawn_record, record_spawn, service_name,
    service_pid, set_service_proc,
};

/// How to invoke n8n: the Node executable plus the package's CLI entry
/// script. We launch `node <pkg>/bin/n8n` rather than the `n8n.cmd` shim
/// because Windows `CreateProcess` can't run a `.cmd` directly and the
/// shim adds a cmd.exe layer between the daemon and the process it
/// supervises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub node: PathBuf,
    pub entry: PathBuf,
}

#[cfg(windows)]
const NODE_NAMES: &[&str] = &["node.exe", "node"];
#[cfg(not(windows))]
const NODE_NAMES: &[&str] = &["node"];

/// Resolve how to launch n8n, honouring overrides:
///   * `WYLDE_N8N_NODE_BIN` — the Node executable.
///   * `WYLDE_N8N_ENTRY` — the n8n CLI entry script (`.../bin/n8n`).
///
/// Otherwise: Node from `PATH`, and the n8n entry discovered from the
/// global npm prefix (`%APPDATA%\npm\node_modules\n8n\bin\n8n` on
/// Windows, `<prefix>/lib/node_modules/n8n/bin/n8n` on Unix).
pub fn resolve_launch() -> Result<LaunchSpec> {
    let node = match std::env::var_os("WYLDE_N8N_NODE_BIN") {
        Some(n) => PathBuf::from(n),
        None => find_on_path(std::env::var_os("PATH").as_deref(), NODE_NAMES)
            .ok_or_else(|| anyhow!("node executable not found on PATH (set WYLDE_N8N_NODE_BIN)"))?,
    };
    let entry = match std::env::var_os("WYLDE_N8N_ENTRY") {
        Some(e) => PathBuf::from(e),
        None => n8n_entry_candidates()
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| {
                anyhow!(
                    "n8n CLI not found — install it (`npm install -g n8n`) or set WYLDE_N8N_ENTRY"
                )
            })?,
    };
    if !entry.exists() {
        return Err(anyhow!(
            "n8n entry script does not exist: {}",
            entry.display()
        ));
    }
    Ok(LaunchSpec { node, entry })
}

/// The conventional global-install locations of the n8n package's
/// `bin/n8n` entry, most specific first.
fn n8n_entry_candidates() -> Vec<PathBuf> {
    let entry_under = |base: PathBuf| base.join("n8n").join("bin").join("n8n");
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates.push(entry_under(
            PathBuf::from(appdata).join("npm").join("node_modules"),
        ));
    }
    // npm prefix-relative (Unix global, and Windows custom prefixes).
    if let Some(prefix) =
        std::env::var_os("NPM_CONFIG_PREFIX").or_else(|| std::env::var_os("npm_config_prefix"))
    {
        let p = PathBuf::from(prefix);
        candidates.push(entry_under(p.join("lib").join("node_modules")));
        candidates.push(entry_under(p.join("node_modules")));
    }
    #[cfg(not(windows))]
    {
        candidates.push(entry_under(PathBuf::from("/usr/local/lib/node_modules")));
        candidates.push(entry_under(PathBuf::from("/usr/lib/node_modules")));
    }
    candidates
}

/// Minimal `which`: the first `dir/name` on `path` that exists.
fn find_on_path(path: Option<&std::ffi::OsStr>, names: &[&str]) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.exists())
}

/// The environment Wylde sets on the engine, on top of the inherited
/// process environment. The single, guarded seam for engine env — screened
/// by [`tests::engine_env_is_loopback_only_private_and_keyed`] rather than
/// sprinkled onto the spawn ad hoc.
pub fn engine_env(
    data_dir: &Path,
    identity: &N8nIdentity,
    port: u16,
) -> Vec<(&'static str, String)> {
    vec![
        // ── persistence: durable, out-of-tree ──────────────────────
        ("N8N_USER_FOLDER", data_dir.display().to_string()),
        ("DB_TYPE", "sqlite".to_owned()),
        // Stable encryption key → saved credentials decrypt across
        // restarts. Load-bearing; never rotate under a live DB.
        ("N8N_ENCRYPTION_KEY", identity.encryption_key.clone()),
        // ── loopback-only network ──────────────────────────────────
        ("N8N_HOST", "127.0.0.1".to_owned()),
        ("N8N_LISTEN_ADDRESS", "127.0.0.1".to_owned()),
        ("N8N_PORT", port.to_string()),
        ("N8N_PROTOCOL", "http".to_owned()),
        // Editor session cookie over plain-http loopback.
        ("N8N_SECURE_COOKIE", "false".to_owned()),
        // ── quiet, private, no outbound calls ───────────────────────
        ("N8N_DIAGNOSTICS_ENABLED", "false".to_owned()),
        ("N8N_VERSION_NOTIFICATIONS_ENABLED", "false".to_owned()),
        ("N8N_PERSONALIZATION_ENABLED", "false".to_owned()),
        ("N8N_HIRING_BANNER_ENABLED", "false".to_owned()),
        ("N8N_TEMPLATES_ENABLED", "false".to_owned()),
        (
            "N8N_USER_MANAGEMENT_EMAIL_NOTIFICATIONS",
            "false".to_owned(),
        ),
    ]
}

/// One cheap TCP probe of the engine's loopback port.
pub fn port_ready(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

/// Boot the n8n engine under the daemon. Every early return logs why.
pub async fn start() -> Result<()> {
    let name = service_name::N8N_ENGINE;
    if is_service_alive(name) {
        let pid = manifest_pid(name)
            .or_else(|| service_pid(name))
            .unwrap_or(0);
        tracing::info!("{name}: already alive (pid={pid}); skipping spawn");
        return Ok(());
    }
    if nospawn_enabled() {
        nospawn_record(name, ImplLang::Rust.as_str());
        tracing::info!("{name}: NO-SPAWN — would-have-spawned recorded; no child forked");
        return Ok(());
    }
    if !wylde_shared::n8n::managed_from_env() {
        tracing::info!("{name}: WYLDE_N8N_MANAGED=0 — n8n is user-managed; not launching it");
        return Ok(());
    }

    // Mint (first run) or load the identity BEFORE anything else, so
    // `wylde-n8n` can attach even when the engine below is external.
    let data_dir = crate::paths::resolve_data_dir(service_name::N8N);
    let identity = match N8nIdentity::load_or_create(&data_dir) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(
                "{name}: cannot load the n8n identity ({e:#}); the engine will NOT start \
                 rather than run under a different encryption key"
            );
            return Ok(());
        }
    };

    let port = wylde_shared::n8n::port_from_env();
    if port_ready(port) {
        tracing::info!(
            "{name}: a daemon already answers on 127.0.0.1:{port} (external instance); \
             skipping spawn"
        );
        return Ok(());
    }
    let spec = match resolve_launch() {
        Ok(spec) => spec,
        Err(e) => {
            tracing::warn!(
                "{name}: {e:#}; the n8n engine will not start — it is optional, so the \
                 rest of the stack is unaffected"
            );
            return Ok(());
        }
    };

    // Bounded capture of the engine's console output, beside its database.
    let logs_dir = data_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).with_context(|| format!("create {}", logs_dir.display()))?;
    let log = wylde_shared::logging::open_rotating_append(
        &logs_dir.join("engine.log"),
        wylde_shared::logging::RotationPolicy::from_env(),
    )
    .with_context(|| "open n8n engine.log")?;
    let log_err = log
        .try_clone()
        .with_context(|| "clone n8n engine.log handle")?;

    let mut cmd = Command::new(&spec.node);
    cmd.arg(&spec.entry)
        .arg("start")
        .current_dir(&data_dir)
        .envs(engine_env(&data_dir, &identity, port))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Own process group, so the generic graceful stop's CTRL_BREAK reaches
    // the engine (and any task-runner child it forks) and nothing else.
    #[cfg(windows)]
    cmd.creation_flags(super::services::CREATE_NEW_PROCESS_GROUP);
    apply_kill_on_drop(&mut cmd);

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn n8n via {}", spec.node.display()))?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(
        "{name}: spawned n8n (pid={pid}) on {} with data dir {}",
        wylde_shared::n8n::base_url(port),
        data_dir.display()
    );
    record_spawn(name, pid, ImplLang::Rust.as_str());
    set_service_proc(name, child);
    Ok(())
}

/// Stop the engine: the generic CTRL_BREAK + wait + force-kill teardown,
/// with a longer grace than a Rust service since n8n flushes its SQLite
/// database and drains running executions on the way out.
pub async fn stop() -> Result<()> {
    stop_service(service_name::N8N_ENGINE, Duration::from_secs(20)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-run random identity, so no key is hard-coded in the tests.
    fn identity() -> N8nIdentity {
        N8nIdentity {
            encryption_key: wylde_shared::rng::byte_array::<32>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            owner_email: "owner@wylde.local".to_owned(),
            owner_password: String::new(),
        }
    }

    #[test]
    fn engine_env_is_loopback_only_private_and_keyed() {
        let id = identity();
        let env = engine_env(Path::new("D:/WyldeData/n8n"), &id, 5999);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("{k} not set"))
        };
        assert_eq!(get("N8N_HOST"), "127.0.0.1");
        assert_eq!(get("N8N_LISTEN_ADDRESS"), "127.0.0.1");
        assert_eq!(get("N8N_PORT"), "5999");
        assert_eq!(get("N8N_ENCRYPTION_KEY"), id.encryption_key);
        assert_eq!(get("N8N_USER_FOLDER"), "D:/WyldeData/n8n");
        for flag in [
            "N8N_DIAGNOSTICS_ENABLED",
            "N8N_VERSION_NOTIFICATIONS_ENABLED",
            "N8N_PERSONALIZATION_ENABLED",
            "N8N_TEMPLATES_ENABLED",
        ] {
            assert_eq!(get(flag), "false", "{flag} must stay off");
        }
        // The owner password is wylde-n8n's to present over REST; it never
        // rides on the engine's environment.
        assert!(env.iter().all(|(k, _)| !k.contains("PASSWORD")));
    }

    #[test]
    fn find_on_path_returns_the_first_existing_candidate() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("node-test-bin"), b"").unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(
            find_on_path(Some(&path), &["node-test-bin"]),
            Some(b.path().join("node-test-bin"))
        );
        assert_eq!(find_on_path(Some(&path), &["absent"]), None);
        assert_eq!(find_on_path(None, &["node-test-bin"]), None);
    }

    #[test]
    fn entry_candidates_all_end_in_the_n8n_cli_script() {
        for c in n8n_entry_candidates() {
            assert!(
                c.ends_with(Path::new("n8n").join("bin").join("n8n")),
                "unexpected candidate {}",
                c.display()
            );
        }
    }
}
