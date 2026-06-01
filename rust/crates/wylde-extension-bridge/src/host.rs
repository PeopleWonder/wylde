//! The per-extension supervisor.
//!
//! Owns the mapping `extension_name -> ExtensionState`, including its
//! live [`McpClient`] connection (when enabled + healthy), its
//! manifest record, and a small event log.
//!
//! `ext.events` (streaming action) subscribes to the broadcast bus
//! this module emits to.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::config::Config;
use crate::manifest::{ExtensionRecord, PanelSource};
use crate::mcp::{McpClient, McpError, SpawnSpec, ToolDescription};

const EVENT_BUS_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Disabled,
    Starting,
    Running,
    Unhealthy,
    Crashed,
    Broken,
}

impl LifecycleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleStatus::Disabled => "disabled",
            LifecycleStatus::Starting => "starting",
            LifecycleStatus::Running => "running",
            LifecycleStatus::Unhealthy => "unhealthy",
            LifecycleStatus::Crashed => "crashed",
            LifecycleStatus::Broken => "broken",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtensionStatus {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub status: LifecycleStatus,
    pub pid: Option<u32>,
    pub negotiated_spec_version: Option<String>,
    pub last_error: Option<String>,
    pub capabilities: Vec<String>,
}

/// One row in the response from `extensions.list_panels`. Flattened
/// so the GUI doesn't have to walk a nested `source.kind` discriminator
/// when there's only one variant today.
#[derive(Debug, Clone, Serialize)]
pub struct PanelEntry {
    pub extension: String,
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub kind: &'static str,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LifecycleEvent {
    pub extension: String,
    pub kind: String, // "spawn" | "exit" | "restart" | "crash" | "disabled" | "enabled" | "unhealthy" | "healthy"
    pub message: String,
    pub at: String,
}

impl LifecycleEvent {
    fn new(extension: &str, kind: &str, message: String) -> Self {
        Self {
            extension: extension.to_owned(),
            kind: kind.to_owned(),
            message,
            at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

struct ExtensionState {
    record: ExtensionRecord,
    status: LifecycleStatus,
    client: Option<Arc<McpClient>>,
    last_error: Option<String>,
    pid: Option<u32>,
    /// Cached tool list (filled at `tools/list` time).
    tool_cache: Option<Vec<ToolDescription>>,
    restart_attempts: u32,
}

impl ExtensionState {
    fn from_record(record: ExtensionRecord) -> Self {
        let status = if record.manifest.enabled {
            LifecycleStatus::Starting
        } else {
            LifecycleStatus::Disabled
        };
        Self {
            record,
            status,
            client: None,
            last_error: None,
            pid: None,
            tool_cache: None,
            restart_attempts: 0,
        }
    }
}

pub struct Host {
    cfg: &'static Config,
    extensions: Arc<RwLock<BTreeMap<String, Mutex<ExtensionState>>>>,
    events: broadcast::Sender<LifecycleEvent>,
}

impl Host {
    pub fn new(cfg: &'static Config) -> Self {
        let (tx, _) = broadcast::channel(EVENT_BUS_CAPACITY);
        Self {
            cfg,
            extensions: Arc::new(RwLock::new(BTreeMap::new())),
            events: tx,
        }
    }

    pub fn event_subscriber(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.events.subscribe()
    }

    /// Reload the extension catalog from disk. Existing live clients
    /// are preserved when their manifest is unchanged.
    pub async fn refresh_catalog(&self) {
        let records = crate::discovery::discover(&self.cfg.extensions_dir);
        let mut state = self.extensions.write().await;
        // Add new / replace changed.
        for (name, rec) in records.iter() {
            state
                .entry(name.clone())
                .or_insert_with(|| Mutex::new(ExtensionState::from_record(rec.clone())));
        }
        // Remove gone-from-disk extensions; drop their clients.
        let known: std::collections::BTreeSet<&String> = records.keys().collect();
        let stale: Vec<String> = state.keys().filter(|k| !known.contains(k)).cloned().collect();
        for name in stale {
            if let Some(mu) = state.remove(&name) {
                let mut s = mu.into_inner();
                if let Some(c) = s.client.take() {
                    if let Ok(client) = Arc::try_unwrap(c) {
                        client.shutdown().await;
                    }
                }
            }
        }
    }

    /// Eagerly start every enabled extension.
    pub async fn start_enabled(&self) {
        let names: Vec<String> = {
            let g = self.extensions.read().await;
            g.keys().cloned().collect()
        };
        for name in names {
            // best-effort; per-extension failures are logged and
            // surfaced via ext.list status.
            if let Err(e) = self.ensure_started(&name).await {
                tracing::warn!("host: ensure_started({}) failed: {}", name, e);
            }
        }
    }

    /// Bring an extension up if its manifest says enabled and it
    /// doesn't have a live client yet. Idempotent.
    pub async fn ensure_started(&self, name: &str) -> Result<(), McpError> {
        let state_map = self.extensions.read().await;
        let mu = state_map
            .get(name)
            .ok_or_else(|| McpError::Transport(format!("unknown extension `{name}`")))?;
        let mut state = mu.lock().await;
        if !state.record.manifest.enabled {
            state.status = LifecycleStatus::Disabled;
            return Ok(());
        }
        if state.client.is_some() && matches!(state.status, LifecycleStatus::Running) {
            return Ok(());
        }
        state.status = LifecycleStatus::Starting;
        let spec = SpawnSpec {
            command: &state.record.manifest.command,
            cwd: Some(&state.record.resolved_cwd()),
            env: &state.record.manifest.env,
        };
        let init_timeout = Duration::from_secs(self.cfg.init_timeout_s);
        match McpClient::connect_stdio(spec, init_timeout, "wylde-extension-bridge").await {
            Ok(client) => {
                state.pid = client.pid();
                state.status = LifecycleStatus::Running;
                state.last_error = None;
                state.restart_attempts = 0;
                let _ = self.events.send(LifecycleEvent::new(
                    name,
                    "spawn",
                    format!(
                        "MCP server `{}` initialized (spec={})",
                        client.server_name, client.negotiated_version
                    ),
                ));
                state.client = Some(Arc::new(client));
                Ok(())
            }
            Err(e) => {
                state.status = LifecycleStatus::Crashed;
                state.last_error = Some(e.to_string());
                state.restart_attempts = state.restart_attempts.saturating_add(1);
                if state.restart_attempts >= self.cfg.restart_max_attempts {
                    state.status = LifecycleStatus::Broken;
                }
                let _ = self.events.send(LifecycleEvent::new(
                    name,
                    "crash",
                    format!("connect_stdio failed: {e}"),
                ));
                Err(e)
            }
        }
    }

    /// Send SIGTERM (or platform equivalent) + drop the client.
    pub async fn stop_one(&self, name: &str) -> Result<(), McpError> {
        let state_map = self.extensions.read().await;
        let mu = state_map
            .get(name)
            .ok_or_else(|| McpError::Transport(format!("unknown extension `{name}`")))?;
        let mut state = mu.lock().await;
        if let Some(client_arc) = state.client.take() {
            // Best-effort: try_unwrap is fine here because we hold
            // the only Arc (no concurrent tools.call should be in
            // flight against a stopping extension — caller's job).
            match Arc::try_unwrap(client_arc) {
                Ok(client) => client.shutdown().await,
                Err(arc) => {
                    // Concurrent call in flight; we drop our reference
                    // and let the last holder shutdown.
                    drop(arc);
                }
            }
        }
        state.status = LifecycleStatus::Disabled;
        state.pid = None;
        state.tool_cache = None;
        let _ = self.events.send(LifecycleEvent::new(
            name,
            "disabled",
            "extension stopped".into(),
        ));
        Ok(())
    }

    /// Test-only: bypass discovery and inject an `ExtensionRecord`
    /// directly into the state map with the supplied lifecycle status.
    /// Lets `list_panels` (and any other pure-read action) be tested
    /// without spawning real MCP children.
    #[doc(hidden)]
    #[cfg(test)]
    pub async fn seed_record_for_tests(&self, record: ExtensionRecord, status: LifecycleStatus) {
        let mut state = self.extensions.write().await;
        let mut s = ExtensionState::from_record(record);
        s.status = status;
        state.insert(s.record.manifest.name.clone(), Mutex::new(s));
    }

    pub async fn shutdown_all(&self) {
        let names: Vec<String> = {
            let g = self.extensions.read().await;
            g.keys().cloned().collect()
        };
        for name in names {
            let _ = self.stop_one(&name).await;
        }
    }

    pub async fn list_status(&self) -> Vec<ExtensionStatus> {
        let g = self.extensions.read().await;
        let mut out = Vec::with_capacity(g.len());
        for (name, mu) in g.iter() {
            let s = mu.lock().await;
            out.push(ExtensionStatus {
                name: name.clone(),
                version: s.record.manifest.version.clone(),
                enabled: s.record.manifest.enabled,
                status: s.status,
                pid: s.pid,
                negotiated_spec_version: s.client.as_ref().map(|c| c.negotiated_version.clone()),
                last_error: s.last_error.clone(),
                capabilities: s.record.capabilities.clone(),
            });
        }
        out
    }

    pub async fn get_status(&self, name: &str) -> Option<ExtensionStatus> {
        let g = self.extensions.read().await;
        let mu = g.get(name)?;
        let s = mu.lock().await;
        Some(ExtensionStatus {
            name: s.record.manifest.name.clone(),
            version: s.record.manifest.version.clone(),
            enabled: s.record.manifest.enabled,
            status: s.status,
            pid: s.pid,
            negotiated_spec_version: s.client.as_ref().map(|c| c.negotiated_version.clone()),
            last_error: s.last_error.clone(),
            capabilities: s.record.capabilities.clone(),
        })
    }

    /// Persist enabled flag + restart the extension's lifecycle.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<ExtensionStatus, McpError> {
        let manifest_path = {
            let g = self.extensions.read().await;
            let mu = g
                .get(name)
                .ok_or_else(|| McpError::Transport(format!("unknown extension `{name}`")))?;
            let s = mu.lock().await;
            s.record.manifest_path.clone()
        };
        crate::manifest::write_enabled(&manifest_path, enabled)
            .map_err(|e| McpError::Transport(format!("persist enabled: {e}")))?;
        crate::discovery::invalidate_cache();
        self.refresh_catalog().await;
        if enabled {
            // Ignore startup failure — caller can inspect via ext.get.
            let _ = self.ensure_started(name).await; // wylde-check: discard-result-ok
            // Broadcast send fails iff there are no subscribers; not an error.
            let _ = self.events.send(LifecycleEvent::new(name, "enabled", "extension enabled".into())); // wylde-check: discard-result-ok
        } else {
            let _ = self.stop_one(name).await; // wylde-check: discard-result-ok
        }
        self.get_status(name)
            .await
            .ok_or_else(|| McpError::Transport(format!("ext `{name}` vanished after toggle")))
    }

    /// Aggregate `tools/list` from every running extension.
    pub async fn aggregate_tools(&self) -> Vec<Value> {
        let names: Vec<String> = {
            let g = self.extensions.read().await;
            g.keys().cloned().collect()
        };
        let timeout = Duration::from_secs(self.cfg.tool_call_timeout_s);
        let mut out: Vec<Value> = Vec::new();
        for name in names {
            let client = {
                let g = self.extensions.read().await;
                let Some(mu) = g.get(&name) else { continue };
                let s = mu.lock().await;
                s.client.clone()
            };
            let Some(client) = client else { continue };
            match client.list_tools(timeout).await {
                Ok(tools) => {
                    for t in tools {
                        out.push(json!({
                            "extension": name,
                            "id": t.name,
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.input_schema,
                            "service": "extension",
                        }));
                    }
                }
                Err(e) => {
                    tracing::warn!("aggregate_tools: ext={} list failed: {}", name, e);
                }
            }
        }
        out
    }

    pub async fn list_tools_for(&self, name: &str) -> Result<Vec<ToolDescription>, McpError> {
        let client = {
            let g = self.extensions.read().await;
            let mu = g
                .get(name)
                .ok_or_else(|| McpError::Transport(format!("unknown extension `{name}`")))?;
            let s = mu.lock().await;
            s.client.clone()
        };
        let client = client
            .ok_or_else(|| McpError::Transport(format!("extension `{name}` not running")))?;
        let timeout = Duration::from_secs(self.cfg.tool_call_timeout_s);
        client.list_tools(timeout).await
    }

    pub async fn call_tool(
        &self,
        extension: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, McpError> {
        // Lazy-start if enabled but not running yet.
        self.ensure_started(extension).await?;
        let client = {
            let g = self.extensions.read().await;
            let mu = g
                .get(extension)
                .ok_or_else(|| McpError::Transport(format!("unknown extension `{extension}`")))?;
            let s = mu.lock().await;
            s.client.clone()
        };
        let client = client
            .ok_or_else(|| McpError::Transport(format!("extension `{extension}` not running")))?;
        let timeout = Duration::from_secs(self.cfg.tool_call_timeout_s);
        client.call_tool(tool, arguments, timeout).await
    }

    pub async fn ping(&self, extension: &str) -> Result<(), McpError> {
        let client = {
            let g = self.extensions.read().await;
            let mu = g
                .get(extension)
                .ok_or_else(|| McpError::Transport(format!("unknown extension `{extension}`")))?;
            let s = mu.lock().await;
            s.client.clone()
        };
        let client = client
            .ok_or_else(|| McpError::Transport(format!("extension `{extension}` not running")))?;
        let timeout = Duration::from_secs(self.cfg.tool_call_timeout_s);
        client.ping(timeout).await
    }

    /// Snapshot every enabled-or-disabled extension's `ui_panels` with
    /// the extension's name attached. Pure read; never spawns a server.
    /// The harness exposes this via the `extensions.list_panels` pipe
    /// action so the GUI can render its Tools tab.
    pub async fn list_panels(&self) -> Vec<PanelEntry> {
        let g = self.extensions.read().await;
        let mut out: Vec<PanelEntry> = Vec::new();
        for (name, mu) in g.iter() {
            let s = mu.lock().await;
            for p in &s.record.ui_panels {
                let PanelSource::Iframe { url } = &p.source;
                out.push(PanelEntry {
                    extension: name.clone(),
                    id: p.id.clone(),
                    title: p.title.clone(),
                    icon: p.icon.clone(),
                    kind: "iframe",
                    url: url.clone(),
                });
            }
        }
        out
    }

    /// Stop + start one extension.
    pub async fn restart(&self, extension: &str) -> Result<ExtensionStatus, McpError> {
        self.stop_one(extension).await?;
        // ensure_started checks enabled flag and is idempotent.
        let _ = self.ensure_started(extension).await;
        let _ = self
            .events
            .send(LifecycleEvent::new(extension, "restart", "restart requested".into()));
        self.get_status(extension)
            .await
            .ok_or_else(|| McpError::Transport(format!("ext `{extension}` vanished after restart")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::manifest::{McpServerManifest, Transport, UiPanel};

    fn make_record(name: &str, panels: Vec<UiPanel>) -> ExtensionRecord {
        let manifest = McpServerManifest {
            name: name.to_owned(),
            description: String::new(),
            version: "1.0".into(),
            enabled: false,
            transport: Transport::Stdio,
            command: vec!["noop".into()],
            cwd: None,
            env: Default::default(),
            url: None,
            capabilities: Vec::new(),
            tools: Vec::new(),
            ui_panels: panels.clone(),
            health: Default::default(),
        };
        ExtensionRecord {
            manifest_path: format!("/tmp/{name}/mcp-server.json").into(),
            root: format!("/tmp/{name}").into(),
            manifest,
            browser_extension_path: None,
            capabilities: Vec::new(),
            ui_panels: panels,
        }
    }

    fn iframe_panel(id: &str, title: &str, url: &str, icon: Option<&str>) -> UiPanel {
        UiPanel {
            id: id.into(),
            title: title.into(),
            icon: icon.map(str::to_owned),
            source: PanelSource::Iframe { url: url.into() },
        }
    }

    #[tokio::test]
    async fn list_panels_unions_across_extensions() {
        let host = Host::new(Config::get());
        host.seed_record_for_tests(
            make_record(
                "n8n",
                vec![iframe_panel(
                    "workflows",
                    "Workflows",
                    "http://127.0.0.1:5678",
                    Some("🔗"),
                )],
            ),
            LifecycleStatus::Disabled,
        )
        .await;
        host.seed_record_for_tests(
            make_record(
                "study",
                vec![
                    iframe_panel("sessions", "Sessions", "http://localhost:9001", None),
                    iframe_panel("history", "History", "http://localhost:9001/h", None),
                ],
            ),
            LifecycleStatus::Running,
        )
        .await;

        let panels = host.list_panels().await;
        assert_eq!(panels.len(), 3);
        // BTreeMap order: extensions sorted by name (n8n, study).
        assert_eq!(panels[0].extension, "n8n");
        assert_eq!(panels[0].id, "workflows");
        assert_eq!(panels[0].title, "Workflows");
        assert_eq!(panels[0].icon.as_deref(), Some("🔗"));
        assert_eq!(panels[0].kind, "iframe");
        assert_eq!(panels[0].url, "http://127.0.0.1:5678");
        assert!(panels.iter().any(|p| p.extension == "study" && p.id == "sessions"));
        assert!(panels.iter().any(|p| p.extension == "study" && p.id == "history"));
    }

    #[tokio::test]
    async fn list_panels_returns_empty_when_no_extensions_declare_panels() {
        let host = Host::new(Config::get());
        host.seed_record_for_tests(make_record("plain", Vec::new()), LifecycleStatus::Disabled)
            .await;
        let panels = host.list_panels().await;
        assert!(panels.is_empty());
    }
}
