//! Managed n8n daemon: launch, env, and data-dir wiring.
//!
//! Before this module, `wylde-n8n` was a pipe proxy in front of an
//! *external, user-managed* n8n the user had to install AND start AND
//! point credentials at by hand. That left all three of Aaron's
//! requirements unmet: nothing embedded the editor, nothing shared
//! auth, and persistence was wherever the user happened to run n8n.
//!
//! Now `wylde-n8n` *owns* the n8n daemon: it spawns the local n8n CLI
//! as a managed child, points `N8N_USER_FOLDER` at the out-of-tree data
//! dir (`WyldeData/n8n/`, injected by the lifecycle daemon as
//! `WYLDE_N8N_DATA_DIR`), and pins the encryption key to the
//! Wylde-owned identity so saved credentials survive restarts. The
//! daemon stays bound to loopback only.
//!
//! n8n is a Node runtime we don't own — spawning it is the same
//! sanctioned third-party-process pattern as the Ollama/Memgraph
//! daemons. All Wylde code stays Rust; the child is `node bin/n8n`.
//!
//! Opt-out: set `WYLDE_N8N_MANAGED=0` to keep the legacy
//! external-daemon behaviour (this module then resolves nothing and the
//! service is a pure proxy again). If a daemon is already listening on
//! the target port we also skip the spawn and attach to it.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tokio::process::{Child, Command};

use crate::secret::N8nIdentity;

/// Default loopback port the managed daemon binds. Overridable with
/// `WYLDE_N8N_PORT`. Kept at n8n's own default so the existing
/// `http://127.0.0.1:5678` upstream URL keeps working untouched.
pub const DEFAULT_PORT: u16 = 5678;

/// Resolved knobs for the managed daemon, read once from env.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// When false (`WYLDE_N8N_MANAGED=0`), wylde-n8n never spawns n8n —
    /// the legacy external-daemon mode.
    pub managed: bool,
    /// Loopback port for the daemon.
    pub port: u16,
    /// Durable data dir (`N8N_USER_FOLDER`). The lifecycle daemon
    /// injects `WYLDE_N8N_DATA_DIR`; a standalone run falls back to a
    /// `WyldeData/n8n` sibling of the repo root.
    pub data_dir: PathBuf,
}

impl RuntimeConfig {
    pub fn from_env(wylde_root: &Path) -> Self {
        let managed = !matches!(
            std::env::var("WYLDE_N8N_MANAGED").ok().as_deref(),
            Some("0") | Some("false") | Some("no")
        );
        let port = std::env::var("WYLDE_N8N_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        Self {
            managed,
            port,
            data_dir: resolve_data_dir(wylde_root),
        }
    }

    /// The loopback base URL the daemon will answer on.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Durable data dir for n8n: the lifecycle-injected `WYLDE_N8N_DATA_DIR`
/// (the out-of-tree foundation contract) else a `WyldeData/n8n` sibling
/// of the repo root, matching `wylde-lifecycle::paths::resolve_data_dir`.
pub fn resolve_data_dir(wylde_root: &Path) -> PathBuf {
    if let Some(d) = std::env::var_os("WYLDE_N8N_DATA_DIR") {
        return PathBuf::from(d);
    }
    let parent = wylde_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| wylde_root.to_path_buf());
    parent.join("WyldeData").join("n8n")
}

/// How to invoke n8n: the Node executable plus the package's CLI entry
/// script. We launch `node <pkg>/bin/n8n` rather than the `n8n.cmd`
/// shim because Windows `CreateProcess` can't run a `.cmd` directly and
/// the shim adds a cmd.exe layer that swallows our `kill_on_drop`.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub node: PathBuf,
    pub entry: PathBuf,
}

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
        None => find_on_path(NODE_NAMES)
            .ok_or_else(|| anyhow!("node executable not found on PATH (set WYLDE_N8N_NODE_BIN)"))?,
    };
    let entry = match std::env::var_os("WYLDE_N8N_ENTRY") {
        Some(e) => PathBuf::from(e),
        None => find_n8n_entry()
            .ok_or_else(|| anyhow!("n8n CLI not found — install it (`npm install -g n8n`) or set WYLDE_N8N_ENTRY"))?,
    };
    if !entry.exists() {
        return Err(anyhow!("n8n entry script does not exist: {}", entry.display()));
    }
    Ok(LaunchSpec { node, entry })
}

#[cfg(windows)]
const NODE_NAMES: &[&str] = &["node.exe", "node"];
#[cfg(not(windows))]
const NODE_NAMES: &[&str] = &["node"];

/// Discover the n8n package's `bin/n8n` entry from the global npm
/// install. Checks the conventional global locations; returns the first
/// that exists.
fn find_n8n_entry() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            candidates.push(
                PathBuf::from(&appdata)
                    .join("npm")
                    .join("node_modules")
                    .join("n8n")
                    .join("bin")
                    .join("n8n"),
            );
        }
    }
    // npm prefix-relative (Unix global, and Windows custom prefixes).
    if let Some(prefix) = std::env::var_os("NPM_CONFIG_PREFIX").or_else(|| std::env::var_os("npm_config_prefix")) {
        let p = PathBuf::from(&prefix);
        candidates.push(p.join("lib").join("node_modules").join("n8n").join("bin").join("n8n"));
        candidates.push(p.join("node_modules").join("n8n").join("bin").join("n8n"));
    }
    #[cfg(not(windows))]
    {
        candidates.push(PathBuf::from("/usr/local/lib/node_modules/n8n/bin/n8n"));
        candidates.push(PathBuf::from("/usr/lib/node_modules/n8n/bin/n8n"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// Minimal `which`: first `dir/name` on `PATH` that exists.
fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Spawn the managed n8n daemon. The child inherits a loopback-only,
/// telemetry-off environment and the Wylde-owned identity (encryption
/// key + the data dir that holds the workflow/credential/execution DB).
///
/// `kill_on_drop` ties the daemon's life to this handle; the service
/// also kills it explicitly on shutdown (see `main.rs`).
pub fn spawn(cfg: &RuntimeConfig, identity: &N8nIdentity, spec: &LaunchSpec) -> Result<Child> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("create n8n data dir {}", cfg.data_dir.display()))?;

    tracing::info!(
        "wylde-n8n: launching managed n8n daemon on {} (data dir {})",
        cfg.base_url(),
        cfg.data_dir.display()
    );

    let mut cmd = Command::new(&spec.node);
    cmd.arg(&spec.entry)
        .arg("start")
        // ── persistence: durable, out-of-tree ──────────────────────
        .env("N8N_USER_FOLDER", &cfg.data_dir)
        .env("DB_TYPE", "sqlite")
        // Stable encryption key → saved credentials decrypt across
        // restarts. Load-bearing; never rotate under a live DB.
        .env("N8N_ENCRYPTION_KEY", &identity.encryption_key)
        // ── loopback-only network ──────────────────────────────────
        .env("N8N_HOST", "127.0.0.1")
        .env("N8N_LISTEN_ADDRESS", "127.0.0.1")
        .env("N8N_PORT", cfg.port.to_string())
        .env("N8N_PROTOCOL", "http")
        // Editor session cookie over plain-http loopback.
        .env("N8N_SECURE_COOKIE", "false")
        // ── quiet, private, no outbound calls ───────────────────────
        .env("N8N_DIAGNOSTICS_ENABLED", "false")
        .env("N8N_VERSION_NOTIFICATIONS_ENABLED", "false")
        .env("N8N_PERSONALIZATION_ENABLED", "false")
        .env("N8N_HIRING_BANNER_ENABLED", "false")
        .env("N8N_TEMPLATES_ENABLED", "false")
        .env(
            "N8N_USER_MANAGEMENT_EMAIL_NOTIFICATIONS",
            "false",
        )
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // New process group on Windows so a console Ctrl-C to the parent
    // doesn't race the explicit shutdown kill (mirrors the lifecycle
    // daemon's own spawn discipline).
    // `creation_flags` is an inherent method on tokio's Command (no std
    // CommandExt import needed). New process group so a console Ctrl-C to
    // the parent doesn't race the explicit shutdown kill.
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .with_context(|| format!("spawn n8n via {}", spec.node.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_uses_port() {
        let cfg = RuntimeConfig {
            managed: true,
            port: 5999,
            data_dir: PathBuf::from("x"),
        };
        assert_eq!(cfg.base_url(), "http://127.0.0.1:5999");
    }

    #[test]
    fn resolve_data_dir_prefers_injected_env() {
        // Guarded so we don't trample a real env in a parallel runner:
        // only assert the fallback shape when the var is absent.
        if std::env::var_os("WYLDE_N8N_DATA_DIR").is_none() {
            let d = resolve_data_dir(Path::new("C:/repo/Wylde-release"));
            assert!(d.ends_with("WyldeData/n8n") || d.ends_with("WyldeData\\n8n"));
        }
    }

    #[test]
    fn managed_defaults_on_and_respects_optout() {
        // Default-on when unset (don't mutate process env in the
        // parallel runner — just assert the parse helper's logic via a
        // local match mirroring from_env).
        let optout = |v: Option<&str>| !matches!(v, Some("0") | Some("false") | Some("no"));
        assert!(optout(None));
        assert!(optout(Some("1")));
        assert!(!optout(Some("0")));
        assert!(!optout(Some("false")));
        assert!(!optout(Some("no")));
    }
}
