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
    UnsupportedSpecVersion { server: String, current: &'static str, prev: &'static str },
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
        let resolved: Vec<String> =
            spec.command.iter().map(|s| resolve_placeholders(s)).collect();
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
            return Err(McpError::Server { code: err.code, message: err.message });
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
            return Err(McpError::Server { code: err.code, message: err.message });
        }
        #[derive(Deserialize)]
        struct ToolsResult { #[serde(default)] tools: Vec<ToolDescription> }
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
            return Err(McpError::Server { code: err.code, message: err.message });
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
            return Err(McpError::Server { code: err.code, message: err.message });
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

/// Substitute `${WYLDE_PYTHON}` / `${WYLDE_ROOT}` in a single argv slot.
///
/// the Wylde user's memory `wylde_py3_resolves_to_python_314` reminds us never
/// to assume `python` on PATH is the .venv interpreter. mcp-server.json
/// files use the `${WYLDE_PYTHON}` token in their command argv so the
/// host can rewrite it to the actual .venv interpreter at spawn time.
/// If the env var is unset, falls back to `<WYLDE_ROOT>/.venv/Scripts/python.exe`
/// on Windows, otherwise to the literal `python3`.
fn resolve_placeholders(s: &str) -> String {
    let mut out = s.to_owned();
    if out.contains("${WYLDE_PYTHON}") {
        let py = std::env::var("WYLDE_PYTHON").unwrap_or_else(|_| default_python());
        out = out.replace("${WYLDE_PYTHON}", &py);
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
