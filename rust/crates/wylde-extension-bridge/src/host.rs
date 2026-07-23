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

/// The claimed-tool partition decision (plan §5): a named tool is hidden
/// from the aggregate catalog **iff** verb mode is active and a resource
/// op claims it. Pure + total so the partition rule is unit-testable
/// without spawning a child MCP server.
fn named_tool_hidden(
    name: &str,
    claimed: &std::collections::BTreeSet<String>,
    verb_mode: bool,
) -> bool {
    verb_mode && claimed.contains(name)
}

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
    /// Live availability — `live`, `unreachable`, or `not_running`
    /// (see [`crate::availability::Availability`]). The GUI renders a
    /// panel as live only on `live`; every other value renders as a
    /// status. Carried per-read so the answer is current, not cached
    /// from process start (#239).
    pub availability: &'static str,
    /// Why the panel isn't live, for the status the GUI shows. `None`
    /// exactly when `availability == "live"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    /// Did this entry come from a filesystem walk?
    ///
    /// `refresh_catalog` prunes entries that discovery no longer finds on
    /// disk — that pruning is what makes a deleted registration vanish
    /// from the GUI without a restart (#239). It must only ever apply to
    /// entries discovery owns: a test-seeded record has no file behind it,
    /// so pruning it would just mean "every read empties the catalog".
    disk_backed: bool,
}

impl ExtensionState {
    fn from_record(record: ExtensionRecord) -> Self {
        // Panel-only extensions (transport=none) have no process
        // lifecycle at all — never "starting", whatever the enabled
        // flag says. Their panels surface through `list_panels`, which
        // is status-independent.
        let status = if record.manifest.transport == crate::manifest::Transport::None {
            LifecycleStatus::Disabled
        } else if record.manifest.enabled {
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
            disk_backed: true,
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
        // Remove gone-from-disk extensions; drop their clients. This is
        // the half that makes a removed registration disappear from the
        // GUI (#239) — without it the catalog only ever grows and a
        // deleted stub keeps its panel until the bridge restarts.
        //
        // Two entries are deliberately exempt:
        //   * ones discovery doesn't own (`disk_backed == false`, i.e.
        //     test-seeded), which have no file to go missing; and
        //   * every entry, when the extensions dir isn't readable at all.
        //     An unreadable directory is *absence of information*, not
        //     evidence that every extension was uninstalled — treating a
        //     transient unreadable mount as "everything is gone" would
        //     blank the whole GUI, a worse silent failure than the one
        //     this fix targets.
        let dir_readable = self.cfg.extensions_dir.is_dir();
        let known: std::collections::BTreeSet<&String> = records.keys().collect();
        let mut stale: Vec<String> = Vec::new();
        if dir_readable {
            // `get_mut` rather than `lock().await`: we already hold the
            // map's write guard, so the per-entry mutexes are provably
            // uncontended and this stays a synchronous read.
            for (name, mu) in state.iter_mut() {
                if !known.contains(name) && mu.get_mut().disk_backed {
                    stale.push(name.clone());
                }
            }
        }
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
        // Panel-only (transport=none): nothing to spawn, nothing to
        // health-ping — short-circuit BEFORE the enabled check so a
        // persisted enabled=true can never fork a ghost process. The
        // extension's panels are already fully served by `list_panels`.
        if state.record.manifest.transport == crate::manifest::Transport::None {
            state.status = LifecycleStatus::Disabled;
            state.last_error = None;
            return Ok(());
        }
        if !state.record.manifest.enabled {
            state.status = LifecycleStatus::Disabled;
            return Ok(());
        }
        if state.client.is_some() && matches!(state.status, LifecycleStatus::Running) {
            return Ok(());
        }
        state.status = LifecycleStatus::Starting;
        // Resolve the working directory (placeholder-substituted) up front so a
        // bad `cwd` fails loud here instead of spawning into a bogus dir.
        let cwd = match state.record.resolved_cwd() {
            Ok(c) => c,
            Err(e) => {
                state.status = LifecycleStatus::Crashed;
                state.last_error = Some(e.clone());
                return Err(McpError::Transport(format!("cwd resolution failed: {e}")));
            }
        };
        let spec = SpawnSpec {
            command: &state.record.manifest.command,
            cwd: Some(&cwd),
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
        // No file backs this record, so `refresh_catalog` must not treat
        // its absence from the filesystem walk as an uninstall.
        s.disk_backed = false;
        state.insert(s.record.manifest.name.clone(), Mutex::new(s));
    }

    /// Test-only: a `Host` whose extensions dir is `dir`.
    ///
    /// Reads now re-walk the filesystem (#239), so a `Host` built from the
    /// ambient [`Config`] would discover whatever is installed on the
    /// developer's machine and fold it into the test's assertions — the
    /// environment-bleed trap. Pointing every test at its own empty temp
    /// dir keeps the catalog exactly what the test seeded.
    ///
    /// Leaks one small `Config` per call to satisfy the `&'static` field;
    /// bounded by the test count and freed at process exit.
    #[doc(hidden)]
    #[cfg(test)]
    pub fn with_extensions_dir_for_tests(dir: std::path::PathBuf) -> Self {
        let mut cfg = Config::get().clone();
        cfg.extensions_dir = dir;
        Self::new(Box::leak(Box::new(cfg)))
    }

    /// Test-only: seed a minimal, never-spawned extension record that
    /// only carries a name + capability list. Lets the inference-gate
    /// tests assert the capability check without building a full manifest
    /// or spawning a child.
    #[doc(hidden)]
    #[cfg(test)]
    pub async fn seed_capabilities_for_tests(&self, name: &str, capabilities: Vec<String>) {
        use crate::manifest::{McpServerManifest, Transport};
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
            capabilities: capabilities.clone(),
            tools: Vec::new(),
            ui_panels: Vec::new(),
            resources: Vec::new(),
            health: Default::default(),
        };
        let record = ExtensionRecord {
            manifest_path: format!("/tmp/{name}/mcp-server.json").into(),
            root: format!("/tmp/{name}").into(),
            manifest,
            browser_extension_path: None,
            capabilities,
            ui_panels: Vec::new(),
        };
        self.seed_record_for_tests(record, LifecycleStatus::Disabled)
            .await;
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
        // Re-walk first so an extension removed from `Extensions/` leaves
        // this list without a bridge restart — the same live-registration
        // property `list_panels` relies on (#239). Cheap when the tree is
        // unchanged (mtime/size-signature cached discovery).
        self.refresh_catalog().await;
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

    /// Does extension `name` declare capability `cap`?
    ///
    /// Reads the resolved `ExtensionRecord.capabilities` (merged from
    /// `mcp-server.json` `capabilities[]` + the legacy `manifest.json`
    /// fallback). Pure read; never spawns. Answers for *disabled*
    /// extensions too — the capability is a static manifest property,
    /// independent of lifecycle. Returns `false` for an unknown
    /// extension.
    ///
    /// This is the enforcement seam for the inference gate
    /// (`actions::inference`): `capabilities[]` graduates from decorative
    /// metadata to the authorization check. The identity is the
    /// self-asserted `extension` field in the request payload — adequate
    /// on the loopback first-party pipe against bugs/misconfig, the same
    /// soft trust model egress already uses (see the security-boundary
    /// design §1.2 / §5.3 for the authenticated-caller stretch upgrade).
    pub async fn extension_has_capability(&self, name: &str, cap: &str) -> bool {
        let g = self.extensions.read().await;
        let Some(mu) = g.get(name) else { return false };
        let s = mu.lock().await;
        s.record.capabilities.iter().any(|c| c == cap)
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
    pub async fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<ExtensionStatus, McpError> {
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
            let _ = self.events.send(LifecycleEvent::new(
                name,
                "enabled",
                "extension enabled".into(),
            )); // wylde-check: discard-result-ok
        } else {
            let _ = self.stop_one(name).await; // wylde-check: discard-result-ok
        }
        self.get_status(name)
            .await
            .ok_or_else(|| McpError::Transport(format!("ext `{name}` vanished after toggle")))
    }

    /// Aggregate `tools/list` from every running extension.
    ///
    /// **Claimed-tool partition (Slice 5a, plan §5):** when verb mode is
    /// active (`WYLDE_HARNESS_VERB_TOOLS`), any tool named by a resource
    /// op in the extension's manifest is **excluded** here — it is
    /// surfaced through the harness verb layer instead, so a claimed tool
    /// never appears in both the named catalog and the resource surface.
    /// With verb mode off (or no `resources[]`), every tool flows as
    /// before.
    pub async fn aggregate_tools(&self) -> Vec<Value> {
        let names: Vec<String> = {
            let g = self.extensions.read().await;
            g.keys().cloned().collect()
        };
        let verb_mode = crate::verb_mode_active();
        let timeout = Duration::from_secs(self.cfg.tool_call_timeout_s);
        let mut out: Vec<Value> = Vec::new();
        for name in names {
            let (client, claimed) = {
                let g = self.extensions.read().await;
                let Some(mu) = g.get(&name) else { continue };
                let s = mu.lock().await;
                (s.client.clone(), s.record.manifest.claimed_tools())
            };
            let Some(client) = client else { continue };
            match client.list_tools(timeout).await {
                Ok(tools) => {
                    for t in tools {
                        if named_tool_hidden(&t.name, &claimed, verb_mode) {
                            // Claimed by a resource op — hidden from the
                            // named catalog (it lives on the verb surface).
                            continue;
                        }
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
        let client =
            client.ok_or_else(|| McpError::Transport(format!("extension `{name}` not running")))?;
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
    /// the extension's name and its live availability attached. Never
    /// spawns a server. The harness exposes this via the
    /// `extensions.list_panels` pipe action so the GUI can render its
    /// Tools tab.
    ///
    /// **Two liveness properties this read guarantees (#239).** Both
    /// exist because the GUI is a projection of the filesystem, and a
    /// projection that answers from a snapshot taken at process start is
    /// not a projection:
    ///
    ///   1. *Registration is re-walked.* [`Self::refresh_catalog`] runs
    ///      first, so an extension deleted from `Extensions/` stops being
    ///      reported on the very next read — no bridge restart, no manual
    ///      file surgery. The walk is cheap: `discovery::discover` is
    ///      cached on an mtime/size signature, so an unchanged tree costs
    ///      a stat pass and no re-parse.
    ///   2. *Availability is probed.* Each panel's URL gets a loopback
    ///      reachability check ([`crate::availability`]), TTL-cached, so
    ///      a registered-but-dead panel is reported `unreachable` rather
    ///      than handed to the GUI as though it worked.
    ///
    /// The probes run **after** the extension lock is released: holding a
    /// per-extension `Mutex` across a network await would serialise every
    /// other bridge action behind this read.
    pub async fn list_panels(&self) -> Vec<PanelEntry> {
        // (1) Live registration.
        self.refresh_catalog().await;

        // Collect the declarations under the locks, then drop them.
        struct Declared {
            extension: String,
            id: String,
            title: String,
            icon: Option<String>,
            url: String,
            panel_only: bool,
            running: bool,
        }
        let declared: Vec<Declared> = {
            let g = self.extensions.read().await;
            let mut rows = Vec::new();
            for (name, mu) in g.iter() {
                let s = mu.lock().await;
                let panel_only = s.record.manifest.transport == crate::manifest::Transport::None;
                let running = matches!(s.status, LifecycleStatus::Running);
                for p in &s.record.ui_panels {
                    let PanelSource::Iframe { url } = &p.source;
                    rows.push(Declared {
                        extension: name.clone(),
                        id: p.id.clone(),
                        title: p.title.clone(),
                        icon: p.icon.clone(),
                        url: url.clone(),
                        panel_only,
                        running,
                    });
                }
            }
            rows
        };

        // (2) Availability, with every lock released.
        let mut out: Vec<PanelEntry> = Vec::with_capacity(declared.len());
        for d in declared {
            let reachable = crate::availability::reachable(&d.url).await;
            let (state, detail) = crate::availability::classify(d.panel_only, d.running, reachable);
            out.push(PanelEntry {
                extension: d.extension,
                id: d.id,
                title: d.title,
                icon: d.icon,
                kind: "iframe",
                url: d.url,
                availability: state.as_str(),
                detail,
            });
        }
        out
    }

    /// Snapshot every (optionally one) extension's declared `resources[]`
    /// for the harness verb-overlay sync (Slice 5a). Pure read; never
    /// spawns a server, so it answers for disabled extensions too (same
    /// property as `list_panels` / `ext.tools.list`'s static path).
    ///
    /// Each row carries the **namespaced** `resource_type`
    /// (`ext:<extension>:<slug>`) the harness registers under, the bare
    /// slug, and the `claimed_tools` set for that resource. The harness
    /// turns each row into a `ResourceDefinition` whose op handlers do one
    /// `ext.tools.call` hop.
    pub async fn list_resource_declarations(&self, only: Option<&str>) -> Vec<Value> {
        let g = self.extensions.read().await;
        let mut out: Vec<Value> = Vec::new();
        for (name, mu) in g.iter() {
            if let Some(want) = only {
                if name != want {
                    continue;
                }
            }
            let s = mu.lock().await;
            for res in &s.record.manifest.resources {
                let mut row = serde_json::to_value(res).unwrap_or_else(|_| json!({}));
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("extension".into(), json!(name));
                    obj.insert("bare_resource_type".into(), json!(res.resource_type));
                    obj.insert(
                        "resource_type".into(),
                        json!(format!("ext:{name}:{}", res.resource_type)),
                    );
                    obj.insert(
                        "claimed_tools".into(),
                        json!(res.claimed_tools().into_iter().collect::<Vec<_>>()),
                    );
                }
                out.push(row);
            }
        }
        out
    }

    /// Stop + start one extension.
    pub async fn restart(&self, extension: &str) -> Result<ExtensionStatus, McpError> {
        self.stop_one(extension).await?;
        // ensure_started checks enabled flag and is idempotent.
        let _ = self.ensure_started(extension).await;
        let _ = self.events.send(LifecycleEvent::new(
            extension,
            "restart",
            "restart requested".into(),
        ));
        self.get_status(extension)
            .await
            .ok_or_else(|| McpError::Transport(format!("ext `{extension}` vanished after restart")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{McpServerManifest, Transport, UiPanel};

    /// A `Host` whose extensions dir is its own empty temp directory.
    ///
    /// Reads re-walk the filesystem now (#239), so a `Host` built from the
    /// ambient config would fold whatever is installed on the developer's
    /// machine into the test's assertions. Hold the returned `TempDir` for
    /// the test's duration — dropping it removes the directory, which
    /// flips `refresh_catalog` into its don't-prune-on-unreadable branch.
    fn hermetic_host() -> (Host, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp extensions dir");
        let host = Host::with_extensions_dir_for_tests(dir.path().to_path_buf());
        (host, dir)
    }

    /// A loopback URL nothing is listening on: bind a port, learn its
    /// number, release it. Beats hardcoding a "probably free" port, which
    /// would make the test pass or fail depending on what the developer
    /// happens to be running.
    async fn dead_loopback_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

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
            resources: Vec::new(),
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
        let (host, _extensions_dir) = hermetic_host();
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
        assert!(panels
            .iter()
            .any(|p| p.extension == "study" && p.id == "sessions"));
        assert!(panels
            .iter()
            .any(|p| p.extension == "study" && p.id == "history"));
    }

    /// Panel-only record: transport=none, no command, panels only —
    /// the Extensions/N8N shape after the TX S3 stub removal.
    fn make_panel_only_record(name: &str, panels: Vec<UiPanel>, enabled: bool) -> ExtensionRecord {
        let mut rec = make_record(name, panels);
        rec.manifest.transport = Transport::None;
        rec.manifest.command = Vec::new();
        rec.manifest.enabled = enabled;
        rec
    }

    #[tokio::test]
    async fn panel_only_ensure_started_spawns_nothing_and_is_ok() {
        // Even with enabled=true persisted, a transport=none extension
        // must never fork a child or report a crash — ensure_started is
        // a clean no-op (status disabled, no client, no pid, no error).
        let (host, _extensions_dir) = hermetic_host();
        host.seed_record_for_tests(
            make_panel_only_record(
                "n8n",
                vec![iframe_panel(
                    "workflows",
                    "Workflows",
                    "http://127.0.0.1:5678",
                    None,
                )],
                true,
            ),
            LifecycleStatus::Disabled,
        )
        .await;

        host.ensure_started("n8n")
            .await
            .expect("panel-only start is a no-op");
        let status = host.get_status("n8n").await.unwrap();
        assert_eq!(status.status, LifecycleStatus::Disabled);
        assert!(status.pid.is_none(), "no process may exist");
        assert!(
            status.last_error.is_none(),
            "no-op must not record an error"
        );

        // …and the panels still surface (status-independent read).
        let panels = host.list_panels().await;
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].extension, "n8n");
        assert_eq!(panels[0].url, "http://127.0.0.1:5678");
    }

    #[test]
    fn panel_only_from_record_never_reports_starting() {
        // enabled=true would normally seed status=Starting; panel-only
        // has no lifecycle, so it must come up Disabled.
        let rec = make_panel_only_record("n8n", Vec::new(), true);
        let state = ExtensionState::from_record(rec);
        assert_eq!(state.status, LifecycleStatus::Disabled);
    }

    #[tokio::test]
    async fn list_panels_returns_empty_when_no_extensions_declare_panels() {
        let (host, _extensions_dir) = hermetic_host();
        host.seed_record_for_tests(make_record("plain", Vec::new()), LifecycleStatus::Disabled)
            .await;
        let panels = host.list_panels().await;
        assert!(panels.is_empty());
    }

    // ────────────────────────────────────────────────────────────────
    // #239 — the GUI is a projection of the filesystem + live health.
    // These drive `list_panels` against a real on-disk extension dir,
    // because the properties under test are exactly the ones an
    // in-memory seeded record cannot exercise.
    // ────────────────────────────────────────────────────────────────

    /// Write a panel-only (`transport: "none"`) extension — the
    /// `Extensions/wylde-images` shape — into `dir`.
    fn write_panel_only_extension(dir: &std::path::Path, name: &str, url: &str) {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).expect("create extension dir");
        let manifest = serde_json::json!({
            "name": name,
            "description": "panel-only stub for tests",
            "version": "1.0",
            "transport": "none",
            "ui_panels": [{
                "id": "gallery",
                "title": "Gallery",
                "source": { "kind": "iframe", "url": url }
            }]
        });
        std::fs::write(
            root.join("mcp-server.json"),
            serde_json::to_string_pretty(&manifest).expect("serialise manifest"),
        )
        .expect("write manifest");
        // Discovery memoises on an (mtime, size) signature; a temp dir
        // created and mutated inside one test can land within the same
        // filesystem timestamp tick, so clear it explicitly rather than
        // depending on clock granularity.
        crate::discovery::invalidate_cache();
    }

    #[tokio::test]
    async fn a_registration_removed_from_disk_stops_being_reported() {
        // The core of #239: deleting the registration must be enough.
        // Before the fix the catalog was walked at bootstrap and on
        // toggle only, so the panel survived every read until the bridge
        // process restarted.
        let (host, extensions_dir) = hermetic_host();
        let url = dead_loopback_url().await;
        write_panel_only_extension(extensions_dir.path(), "wylde-images-test", &url);

        let before = host.list_panels().await;
        assert_eq!(
            before.len(),
            1,
            "the declared panel is reported while registered"
        );
        assert_eq!(before[0].extension, "wylde-images-test");

        // Remove the registration — no restart, no toggle, no other call.
        std::fs::remove_dir_all(extensions_dir.path().join("wylde-images-test"))
            .expect("remove extension dir");
        crate::discovery::invalidate_cache();

        let after = host.list_panels().await;
        assert!(
            after.is_empty(),
            "a removed registration must vanish on the next read, not at the next restart; got {after:?}"
        );
        // And it leaves the extension list too, not just the panel list.
        assert!(
            host.list_status()
                .await
                .iter()
                .all(|e| e.name != "wylde-images-test"),
            "the extension itself also leaves ext.list"
        );
    }

    #[tokio::test]
    async fn a_registered_panel_with_a_dead_port_is_unreachable_never_live() {
        // The dead-Images case: registration present, service gone. The
        // panel must be reported with a status, not handed to the GUI as
        // though it worked.
        let (host, extensions_dir) = hermetic_host();
        let dead = dead_loopback_url().await;
        write_panel_only_extension(extensions_dir.path(), "dead-panel-ext", &dead);
        crate::availability::invalidate_cache();

        let panels = host.list_panels().await;
        assert_eq!(panels.len(), 1);
        assert_eq!(
            panels[0].availability, "unreachable",
            "a registered panel pointing at a dead port is unreachable, not live"
        );
        assert!(
            panels[0].detail.is_some(),
            "an unavailable panel must carry a reason the GUI can show"
        );
        // The URL is still reported — the GUI shows what it points at.
        assert_eq!(panels[0].url, dead);
    }

    #[tokio::test]
    async fn a_registered_panel_with_a_live_listener_is_live() {
        // The other side of the gate: availability tracks reality, so a
        // panel-only extension whose port IS up reports live even though
        // it has no process and is permanently "disabled".
        let (host, extensions_dir) = hermetic_host();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let url = format!("http://127.0.0.1:{port}");
        write_panel_only_extension(extensions_dir.path(), "live-panel-ext", &url);
        crate::availability::invalidate_cache();

        let panels = host.list_panels().await;
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].availability, "live");
        assert!(
            panels[0].detail.is_none(),
            "a live panel carries no unavailability reason"
        );
    }

    #[tokio::test]
    async fn an_unreadable_extensions_dir_does_not_wipe_the_catalog() {
        // Absence of information is not evidence of uninstall: if the
        // extensions dir can't be read we keep what we know rather than
        // blanking every panel in the GUI.
        let (host, extensions_dir) = hermetic_host();
        let url = dead_loopback_url().await;
        write_panel_only_extension(extensions_dir.path(), "survives-ext", &url);
        assert_eq!(host.list_panels().await.len(), 1);

        // Drop the whole directory — the dir itself is now gone, which is
        // "unreadable", not "empty".
        drop(extensions_dir);
        crate::discovery::invalidate_cache();

        assert_eq!(
            host.list_panels().await.len(),
            1,
            "an unreadable extensions dir must not be read as 'everything was uninstalled'"
        );
    }

    #[test]
    fn partition_hides_only_claimed_tools_and_only_in_verb_mode() {
        let mut claimed = std::collections::BTreeSet::new();
        claimed.insert("fetch".to_string());
        claimed.insert("scrape".to_string());

        // Verb mode ON: claimed tools are hidden, unclaimed stay visible.
        assert!(named_tool_hidden("fetch", &claimed, true));
        assert!(named_tool_hidden("scrape", &claimed, true));
        assert!(!named_tool_hidden("other", &claimed, true));

        // Verb mode OFF: nothing is hidden — named-tool behaviour intact.
        assert!(!named_tool_hidden("fetch", &claimed, false));
        assert!(!named_tool_hidden("other", &claimed, false));
    }

    #[tokio::test]
    async fn list_resource_declarations_namespaces_and_attaches_claimed() {
        use crate::manifest::{
            ActionDeclaration, McpServerManifest, OperationDeclaration, ResourceDeclaration,
            Transport,
        };
        let mut ops = std::collections::BTreeMap::new();
        ops.insert(
            "execute".to_string(),
            OperationDeclaration {
                description: "web".into(),
                mcp_tool: String::new(),
                destructive: false,
                tier: "read".into(),
                actions: vec![
                    ActionDeclaration {
                        name: "fetch".into(),
                        description: String::new(),
                        mcp_tool: Some("fetch".into()),
                        destructive: false,
                    },
                    ActionDeclaration {
                        name: "scrape".into(),
                        description: String::new(),
                        mcp_tool: Some("scrape".into()),
                        destructive: false,
                    },
                ],
                args_schema: serde_json::Value::Null,
                response_schema: serde_json::Value::Null,
            },
        );
        let manifest = McpServerManifest {
            name: "Webcrawler".into(),
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
            ui_panels: Vec::new(),
            resources: vec![ResourceDeclaration {
                resource_type: "url".into(),
                display_name: "Web URL".into(),
                description: "d".into(),
                scope: "global".into(),
                identifier_fields: Vec::new(),
                filter_fields: Vec::new(),
                schema_version: 1,
                operations: ops,
            }],
            health: Default::default(),
        };
        let record = ExtensionRecord {
            manifest_path: "/tmp/Webcrawler/mcp-server.json".into(),
            root: "/tmp/Webcrawler".into(),
            manifest,
            browser_extension_path: None,
            capabilities: Vec::new(),
            ui_panels: Vec::new(),
        };
        let (host, _extensions_dir) = hermetic_host();
        host.seed_record_for_tests(record, LifecycleStatus::Disabled)
            .await;

        let rows = host.list_resource_declarations(None).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["extension"], "Webcrawler");
        assert_eq!(row["resource_type"], "ext:Webcrawler:url");
        assert_eq!(row["bare_resource_type"], "url");
        let claimed: Vec<&str> = row["claimed_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(claimed, vec!["fetch", "scrape"]);
        // Per-extension filter excludes others.
        assert!(host
            .list_resource_declarations(Some("Other"))
            .await
            .is_empty());
    }
}
