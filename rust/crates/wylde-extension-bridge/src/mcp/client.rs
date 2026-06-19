//! High-level MCP client: spawn a stdio MCP server child process,
//! perform the `initialize` handshake, drive `tools/list` /
//! `tools/call` / `ping`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::process::Command;

use crate::config::{MCP_SPEC_VERSION, MCP_SPEC_VERSION_PREV};
use crate::version::{classify, VersionDecision};

use super::stdio::StdioConn;
use super::wire::{build_initialize_params, InitializeResult, Notification, Request};

#[derive(Debug, Error)]
pub enum McpError {
    #[error("failed to spawn MCP server: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("MCP transport error: {0}")]
    Transport(String),
    #[error("MCP initialize timed out after {0:?}")]
    InitTimeout(Duration),
    #[error("MCP call timed out after {0:?}")]
    CallTimeout(Duration),
    #[error("MCP server reported spec version {server:?}; host accepts only {current} or {prev}")]
    UnsupportedSpecVersion {
        server: String,
        current: &'static str,
        prev: &'static str,
    },
    #[error("MCP server returned error: code={code} message={message}")]
    Server { code: i64, message: String },
    #[error("decode error: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolDescription {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// One live MCP client connection.
pub struct McpClient {
    pub server_name: String,
    pub negotiated_version: String,
    pub version_decision: VersionDecision,
    conn: StdioConn,
}

#[derive(Debug, Clone)]
pub struct SpawnSpec<'a> {
    pub command: &'a [String],
    pub cwd: Option<&'a Path>,
    pub env: &'a std::collections::BTreeMap<String, String>,
}

impl McpClient {
    /// Spawn the child, perform `initialize`, send
    /// `notifications/initialized`, return the live client.
    pub async fn connect_stdio(
        spec: SpawnSpec<'_>,
        init_timeout: Duration,
        client_name: &str,
    ) -> Result<Self, McpError> {
        let resolved: Vec<String> = spec
            .command
            .iter()
            .map(|s| resolve_placeholders(s))
            .collect();
        let (program, args) = resolved
            .split_first()
            .ok_or_else(|| McpError::Transport("empty command argv".into()))?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // captured by parent log surface
            .kill_on_drop(true);
        if let Some(cwd) = spec.cwd {
            cmd.current_dir(cwd);
        }
        // ── least-privilege env scrub (security boundary §4) ────────────
        // Historically the child INHERITED the bridge's full environment
        // (no `env_clear()`), so any extension could read every var the
        // bridge held — secrets, unrelated service endpoints, dev overrides
        // (`WYLDE_*_BIN`, `WYLDE_DEV_HOTRELOAD`). We now drop the inherited
        // environment and re-add only a minimal allowlist (OS/DLL-load
        // essentials + the Wylde root/bin anchors + the per-service data-dir
        // convention). The manifest's own `env` block is layered on top
        // afterwards and always wins. Rollback hatch: `WYLDE_BRIDGE_SCRUB_ENV=0`
        // restores full inheritance for a field extension that needs a var
        // this allowlist drops.
        if env_scrub_enabled() {
            cmd.env_clear();
            for (k, v) in std::env::vars_os() {
                if k.to_str().map(is_allowed_env).unwrap_or(false) {
                    cmd.env(&k, &v);
                }
            }
        }
        // Manifest-declared env: injected in BOTH modes, last, so it wins.
        for (k, v) in spec.env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().map_err(McpError::Spawn)?;
        // Surface child stderr in the host's tracing log without
        // interleaving with JSON frames (which arrive on stdout).
        // best-effort; not all child types have stderr captured.
        let conn = StdioConn::attach(child)
            .map_err(|e| McpError::Transport(format!("attach stdio: {e}")))?;

        // ── initialize ──────────────────────────────────────────────
        let id = conn.next_id().await;
        let req = Request::new(
            id,
            "initialize",
            build_initialize_params(client_name, env!("CARGO_PKG_VERSION"), MCP_SPEC_VERSION),
        );
        let resp = tokio::time::timeout(init_timeout, conn.send_request(req))
            .await
            .map_err(|_| McpError::InitTimeout(init_timeout))?
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(McpError::Server {
                code: err.code,
                message: err.message,
            });
        }
        let init: InitializeResult = serde_json::from_value(resp.result.unwrap_or(Value::Null))
            .map_err(|e| McpError::Decode(e.to_string()))?;
        let decision = classify(&init.protocol_version);
        if !decision.accepted() {
            tracing::warn!(
                target: "wylde_extension_bridge::mcp",
                server = %init.server_info.name,
                server_version = %init.protocol_version,
                host_version = %MCP_SPEC_VERSION,
                host_prev_version = %MCP_SPEC_VERSION_PREV,
                decision = %decision.as_str(),
                "MCP spec version rejected by per-extension compat policy (N/N-1/N+1)"
            );
            return Err(McpError::UnsupportedSpecVersion {
                server: init.protocol_version,
                current: MCP_SPEC_VERSION,
                prev: MCP_SPEC_VERSION_PREV,
            });
        }
        // Required `notifications/initialized` to signal handshake done.
        conn.send_notification(Notification::new("notifications/initialized", json!({})))
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        Ok(Self {
            server_name: if init.server_info.name.is_empty() {
                "unknown".into()
            } else {
                init.server_info.name
            },
            negotiated_version: init.protocol_version,
            version_decision: decision,
            conn,
        })
    }

    /// `tools/list` — returns the server's tool catalog.
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<ToolDescription>, McpError> {
        let id = self.conn.next_id().await;
        let req = Request::new(id, "tools/list", json!({}));
        let resp = tokio::time::timeout(timeout, self.conn.send_request(req))
            .await
            .map_err(|_| McpError::CallTimeout(timeout))?
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(McpError::Server {
                code: err.code,
                message: err.message,
            });
        }
        #[derive(Deserialize)]
        struct ToolsResult {
            #[serde(default)]
            tools: Vec<ToolDescription>,
        }
        let parsed: ToolsResult = serde_json::from_value(resp.result.unwrap_or(json!({})))
            .map_err(|e| McpError::Decode(e.to_string()))?;
        Ok(parsed.tools)
    }

    /// `tools/call` — invoke a tool with arguments. Returns the
    /// server's raw result object.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = self.conn.next_id().await;
        let req = Request::new(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let resp = tokio::time::timeout(timeout, self.conn.send_request(req))
            .await
            .map_err(|_| McpError::CallTimeout(timeout))?
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(McpError::Server {
                code: err.code,
                message: err.message,
            });
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    /// `ping` — used by the host's health check loop.
    pub async fn ping(&self, timeout: Duration) -> Result<(), McpError> {
        let id = self.conn.next_id().await;
        let req = Request::new(id, "ping", json!({}));
        let resp = tokio::time::timeout(timeout, self.conn.send_request(req))
            .await
            .map_err(|_| McpError::CallTimeout(timeout))?
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(McpError::Server {
                code: err.code,
                message: err.message,
            });
        }
        Ok(())
    }

    pub async fn shutdown(self) {
        self.conn.shutdown().await;
    }

    /// OS pid of the spawned child process, if known.
    pub fn pid(&self) -> Option<u32> {
        self.conn.child.id()
    }
}

/// Substitute `${WYLDE_PYTHON}` / `${WYLDE_ROOT}` / `${WYLDE_BIN}` in a single
/// argv slot.
///
/// the Wylde user's memory `wylde_py3_resolves_to_python_314` reminds us never
/// to assume `python` on PATH is the .venv interpreter. mcp-server.json
/// files use the `${WYLDE_PYTHON}` token in their command argv so the
/// host can rewrite it to the actual .venv interpreter at spawn time.
/// If the env var is unset, falls back to `<WYLDE_ROOT>/.venv/Scripts/python.exe`
/// on Windows, otherwise to the literal `python3`.
///
/// **`${WYLDE_PYTHON}` is DEPRECATED — test-shim only since 2026-06-11.**
/// Its last production user (Extensions/N8N's Python-stub command) went
/// panel-only (`transport: "none"`) in taxonomy reorg TX S3; no
/// production manifest may use the token any more. The substitution
/// survives solely for the bridge integration test's `Extensions/_shim`
/// Python MCP server (`tests/integration.rs`). Rust-native extensions
/// use `${WYLDE_BIN}`.
///
/// `${WYLDE_BIN}` resolves to the directory holding the built Rust service
/// binaries so a manifest can point its `command` at a native sidecar — e.g.
/// `["${WYLDE_BIN}/wylde-ext-webcrawler.exe"]` — exactly as the Python
/// extensions point at `${WYLDE_PYTHON}` today. This is the host-side
/// prerequisite for the legacy-extensions Rust rewrite (Slice 3 flips the
/// Webcrawler manifest to use it); see
/// `docs/plans/legacy-extensions-rust-rewrite.md`. If `WYLDE_BIN` is unset it
/// falls back to `<WYLDE_ROOT>/rust/target/release`.
fn resolve_placeholders(s: &str) -> String {
    let mut out = s.to_owned();
    if out.contains("${WYLDE_PYTHON}") {
        let py = std::env::var("WYLDE_PYTHON").unwrap_or_else(|_| default_python());
        out = out.replace("${WYLDE_PYTHON}", &py);
    }
    if out.contains("${WYLDE_BIN}") {
        let bin = std::env::var("WYLDE_BIN").unwrap_or_else(|_| default_bin_dir());
        out = out.replace("${WYLDE_BIN}", &bin);
    }
    if out.contains("${WYLDE_ROOT}") {
        let root = std::env::var("WYLDE_ROOT").unwrap_or_else(|_| ".".to_string());
        out = out.replace("${WYLDE_ROOT}", &root);
    }
    out
}

fn default_python() -> String {
    let root = std::env::var("WYLDE_ROOT").unwrap_or_else(|_| ".".to_string());
    if cfg!(windows) {
        format!("{root}\\.venv\\Scripts\\python.exe")
    } else {
        format!("{root}/.venv/bin/python3")
    }
}

/// Default directory for built Rust service binaries when `WYLDE_BIN` is unset:
/// `<WYLDE_ROOT>/rust/target/release` (the cargo release output dir).
fn default_bin_dir() -> String {
    let root = std::env::var("WYLDE_ROOT").unwrap_or_else(|_| ".".to_string());
    if cfg!(windows) {
        format!("{root}\\rust\\target\\release")
    } else {
        format!("{root}/rust/target/release")
    }
}

// ── least-privilege spawn-env scrub (security boundary §4) ───────────────

/// Whether the spawn-time environment scrub is active. ON by default — the
/// least-privilege boundary is the point. The rollback hatch
/// `WYLDE_BRIDGE_SCRUB_ENV=0|false|off|no` restores the legacy "inherit the
/// bridge's full environment" behaviour if a field extension turns out to need
/// a var this allowlist drops.
fn env_scrub_enabled() -> bool {
    match std::env::var("WYLDE_BRIDGE_SCRUB_ENV") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// Environment variable NAMES a spawned extension may inherit from the bridge
/// under the least-privilege scrub. Everything else is dropped; an extension
/// that needs more must declare it in its manifest `env` block (injected on top,
/// so it always wins). Names are compared case-insensitively because Windows
/// preserves the original case of env keys (`Path`, `SystemRoot`, …).
///
/// The set is deliberately minimal: (a) the Wylde root/bin anchors an extension
/// uses to resolve its data dir and locate sibling binaries, and (b) the OS
/// variables a process needs to launch and load DLLs on Windows (plus the POSIX
/// equivalents, harmless here, that keep the bridge portable). It carries NO
/// secret-bearing var: API keys, tokens, dev overrides (`WYLDE_*_BIN`,
/// `WYLDE_DEV_HOTRELOAD`) and unrelated service config all fail the allowlist.
const ENV_ALLOWLIST: &[&str] = &[
    // ── Wylde platform anchors ──
    "WYLDE_ROOT", // resolve data dir + first-party paths
    "WYLDE_BIN",  // locate sibling service binaries
    // ── Windows process / DLL-load essentials ──
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "PATH",
    "PATHEXT",
    "COMSPEC",
    "TEMP",
    "TMP",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_ARCHITEW6432",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "USERNAME",
    "USERPROFILE",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "APPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "SESSIONNAME",
    // ── POSIX essentials (harmless on Windows; keeps the bridge portable) ──
    "HOME",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
];

/// Does the bridge pass env var `name` through to a spawned extension under the
/// scrub? True for the fixed [`ENV_ALLOWLIST`] (case-insensitive) and for the
/// per-service data-dir convention `WYLDE_<SVC>_DATA_DIR` — a user-owned
/// library path (not a secret) — so a data-owning extension keeps its corpus
/// location across the scrub.
fn is_allowed_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ENV_ALLOWLIST.contains(&upper.as_str())
        || (upper.starts_with("WYLDE_") && upper.ends_with("_DATA_DIR"))
}

/// Pure core of the spawn-time env scrub, factored out so it can be unit-tested
/// without spawning a child. Mirrors `connect_stdio` exactly: when `scrub`,
/// filter the bridge's environment to the allowlist; then overlay the manifest's
/// declared `env` (always, and it wins).
#[cfg(test)]
fn child_env(
    parent: impl IntoIterator<Item = (String, String)>,
    declared: &std::collections::BTreeMap<String, String>,
    scrub: bool,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (k, v) in parent {
        if !scrub || is_allowed_env(&k) {
            out.insert(k, v);
        }
    }
    for (k, v) in declared {
        out.insert(k.clone(), v.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::resolve_placeholders;

    #[test]
    fn wylde_bin_token_resolves_to_env_when_set() {
        // SAFETY: single-threaded test; restore after.
        std::env::set_var("WYLDE_BIN", "/opt/wylde/bin");
        let out = resolve_placeholders("${WYLDE_BIN}/wylde-ext-webcrawler");
        assert_eq!(out, "/opt/wylde/bin/wylde-ext-webcrawler");
        std::env::remove_var("WYLDE_BIN");
    }

    #[test]
    fn wylde_bin_token_falls_back_to_release_dir() {
        std::env::remove_var("WYLDE_BIN");
        std::env::set_var("WYLDE_ROOT", "/repo");
        let out = resolve_placeholders("${WYLDE_BIN}/x");
        let want = if cfg!(windows) {
            "/repo\\rust\\target\\release/x"
        } else {
            "/repo/rust/target/release/x"
        };
        assert_eq!(out, want);
        std::env::remove_var("WYLDE_ROOT");
    }

    #[test]
    fn argv_without_tokens_is_unchanged() {
        assert_eq!(resolve_placeholders("plain/arg --flag"), "plain/arg --flag");
    }

    // ── least-privilege env scrub ────────────────────────────────────────
    use super::{child_env, is_allowed_env};
    use std::collections::BTreeMap;

    #[test]
    fn allowlist_keeps_anchors_and_os_essentials_case_insensitively() {
        // Wylde anchors.
        assert!(is_allowed_env("WYLDE_ROOT"));
        assert!(is_allowed_env("WYLDE_BIN"));
        // OS essentials, in the exact case Windows hands us.
        assert!(is_allowed_env("Path"));
        assert!(is_allowed_env("SystemRoot"));
        assert!(is_allowed_env("windir"));
        assert!(is_allowed_env("TEMP"));
        // Per-service data-dir convention (a path, not a secret).
        assert!(is_allowed_env("WYLDE_STUDY_DATA_DIR"));
        assert!(is_allowed_env("wylde_voice_data_dir"));
    }

    #[test]
    fn allowlist_drops_secrets_and_dev_overrides() {
        // Secrets / unrelated config must NOT cross the boundary.
        assert!(!is_allowed_env("WYLDE_OLLAMA_API_KEY"));
        assert!(!is_allowed_env("OPENAI_API_KEY"));
        assert!(!is_allowed_env("AWS_SECRET_ACCESS_KEY"));
        assert!(!is_allowed_env("GITHUB_TOKEN"));
        // Dev-loop machinery the extension has no business seeing.
        assert!(!is_allowed_env("WYLDE_DEV_HOTRELOAD"));
        assert!(!is_allowed_env("WYLDE_EXTENSION_BRIDGE_BIN"));
        assert!(!is_allowed_env("WYLDE_GATEWAY_BIN"));
        // `WYLDE_STUDY_DATA` (no `_DIR`) is NOT the convention var — dropped;
        // the extension falls back to WYLDE_ROOT, or declares it in manifest.
        assert!(!is_allowed_env("WYLDE_STUDY_DATA"));
    }

    #[test]
    fn scrub_filters_parent_env_to_allowlist() {
        let parent = vec![
            ("WYLDE_ROOT".to_string(), "C:/wylde".to_string()),
            ("Path".to_string(), "C:/windows".to_string()),
            ("WYLDE_OLLAMA_API_KEY".to_string(), "s3cr3t".to_string()),
            ("WYLDE_DEV_HOTRELOAD".to_string(), "1".to_string()),
            ("RANDOM_BRIDGE_VAR".to_string(), "leak".to_string()),
        ];
        let declared = BTreeMap::new();
        let env = child_env(parent, &declared, true);
        // Allowlisted kept …
        assert_eq!(env.get("WYLDE_ROOT").map(String::as_str), Some("C:/wylde"));
        assert!(env.contains_key("Path"));
        // … everything else dropped.
        assert!(!env.contains_key("WYLDE_OLLAMA_API_KEY"));
        assert!(!env.contains_key("WYLDE_DEV_HOTRELOAD"));
        assert!(!env.contains_key("RANDOM_BRIDGE_VAR"));
    }

    #[test]
    fn manifest_env_is_layered_on_top_and_wins() {
        let parent = vec![("WYLDE_ROOT".to_string(), "C:/wylde".to_string())];
        let declared: BTreeMap<String, String> = [
            // A var the allowlist would drop, but the manifest declares.
            (
                "WYLDE_STUDY_EMBED_MODEL".to_string(),
                "nomic-embed-text".to_string(),
            ),
            // Overrides an allowlisted value.
            ("WYLDE_ROOT".to_string(), "C:/override".to_string()),
        ]
        .into_iter()
        .collect();
        let env = child_env(parent, &declared, true);
        assert_eq!(
            env.get("WYLDE_STUDY_EMBED_MODEL").map(String::as_str),
            Some("nomic-embed-text"),
            "declared env survives the scrub"
        );
        assert_eq!(
            env.get("WYLDE_ROOT").map(String::as_str),
            Some("C:/override"),
            "manifest env wins over the inherited value"
        );
    }

    /// LIVE process-boundary proof (Windows): spawn a real child with the
    /// exact scrub `connect_stdio` applies (`env_clear()` + [`is_allowed_env`]
    /// filter) and inspect the child's ACTUAL environment via `cmd /c set`.
    /// A planted bridge secret must be absent from the child; `SystemRoot`
    /// (allowlisted) must be present. Ignored by default — run during live
    /// verification with `--ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore = "spawns a real child; run explicitly during live verification"]
    fn live_spawn_scrubs_secret_keeps_systemroot() {
        use std::process::Command;
        // Plant a non-allowlisted secret in THIS process's environment, the
        // way a bridge daemon would hold one.
        // SAFETY: single-threaded test scope.
        std::env::set_var("WYLDE_BRIDGE_LIVE_SECRET", "should-not-leak");

        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "set"]);
        // Mirror connect_stdio's scrub exactly.
        cmd.env_clear();
        for (k, v) in std::env::vars_os() {
            if k.to_str().map(is_allowed_env).unwrap_or(false) {
                cmd.env(&k, &v);
            }
        }
        let out = cmd.output().expect("spawn cmd /c set");
        let dump = String::from_utf8_lossy(&out.stdout).to_uppercase();

        std::env::remove_var("WYLDE_BRIDGE_LIVE_SECRET");

        assert!(
            !dump.contains("WYLDE_BRIDGE_LIVE_SECRET"),
            "the planted secret leaked into the spawned child's environment"
        );
        assert!(
            dump.contains("SYSTEMROOT="),
            "SystemRoot (allowlisted) must survive so the child can launch"
        );
    }

    #[test]
    fn scrub_disabled_inherits_everything_plus_declared() {
        let parent = vec![
            ("WYLDE_OLLAMA_API_KEY".to_string(), "s3cr3t".to_string()),
            ("RANDOM_BRIDGE_VAR".to_string(), "kept".to_string()),
        ];
        let declared = BTreeMap::new();
        let env = child_env(parent, &declared, false);
        // Rollback hatch: full inheritance, the legacy behaviour.
        assert!(env.contains_key("WYLDE_OLLAMA_API_KEY"));
        assert!(env.contains_key("RANDOM_BRIDGE_VAR"));
    }
}
