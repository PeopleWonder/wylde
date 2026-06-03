//! `mcp-server.json` manifest schema and parser.
//!
//! Each extension declares its MCP-server config under
//! `<extensions_dir>/<extension>/mcp-server.json`. This file describes
//! how to start the server, what transport to use, and (optionally) a
//! declarative tool catalog the host can advertise without first
//! spawning the server.
//!
//! Backward compatibility: the legacy `manifest.json` (parsed by the
//! Python `Extensions.extension_bridge.contract`) is read alongside
//! `mcp-server.json` to harvest the `capabilities` block (egress.*,
//! ingress.*) and `browser_extension_path`, both of which still feed
//! Gateway allowlist / browser-extension handling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("manifest not found: {0}")]
    NotFound(PathBuf),
    #[error("manifest read failed for {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("manifest parse failed for {path}: {source}")]
    Parse { path: PathBuf, source: serde_json::Error },
    #[error("manifest validation failed for {path}: {message}")]
    Validation { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// JSON-RPC over child-process stdin/stdout — the MCP default.
    Stdio,
    /// JSON-RPC over a local HTTP(S) endpoint. Not implemented in this
    /// phase; included so manifests with `"transport": "http"` parse
    /// (and validation rejects them with a clear error).
    Http,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthConfig {
    /// MCP method used for the periodic liveness check. Defaults to
    /// `"ping"`.
    #[serde(default = "default_ping")]
    pub method: String,
    #[serde(default = "default_health_interval_s")]
    pub interval_s: u64,
    #[serde(default = "default_health_timeout_s")]
    pub timeout_s: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            method: default_ping(),
            interval_s: default_health_interval_s(),
            timeout_s: default_health_timeout_s(),
        }
    }
}

fn default_ping() -> String { "ping".to_string() }
fn default_health_interval_s() -> u64 { 30 }
fn default_health_timeout_s() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DeclaredTool {
    pub tool_id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub version: Option<String>,
}

/// The seven generic resource verbs an extension may declare an
/// `operations` entry for. Mirrors `wylde-harness`'s
/// `tooling::resource::ResourceOp` — kept as a bare const set here so the
/// bridge can validate `operations` keys without depending on the harness
/// crate (the two communicate over the JSON wire, not by type sharing).
pub const RESOURCE_VERBS: [&str; 7] =
    ["list", "get", "create", "update", "delete", "search", "execute"];

/// One declared resource (tool-registry consolidation Slice 5a,
/// `docs/plans/extension-resource-declaration.md` §2.2). `resource_type`
/// is the bare slug; the harness namespaces it to
/// `ext:<extension>:<resource_type>` at registration so collisions across
/// extensions are structurally impossible (R-COLLIDE).
///
/// Backwards-compatible by construction: an extension with no `resources`
/// field deserialises to an empty vec and keeps today's named-tool
/// behaviour unchanged.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ResourceDeclaration {
    pub resource_type: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub identifier_fields: Vec<String>,
    #[serde(default)]
    pub filter_fields: Vec<String>,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Keyed by verb (`list`/`get`/…/`execute`). A `BTreeMap` so the
    /// wire order is deterministic.
    pub operations: BTreeMap<String, OperationDeclaration>,
}

impl ResourceDeclaration {
    /// MCP tool names this single resource claims (op `mcp_tool`s + any
    /// per-action overrides).
    pub fn claimed_tools(&self) -> std::collections::BTreeSet<String> {
        let mut claimed = std::collections::BTreeSet::new();
        for op in self.operations.values() {
            if !op.mcp_tool.is_empty() {
                claimed.insert(op.mcp_tool.clone());
            }
            for act in &op.actions {
                if let Some(t) = &act.mcp_tool {
                    if !t.is_empty() {
                        claimed.insert(t.clone());
                    }
                }
            }
        }
        claimed
    }
}

/// One verb's binding to a concrete MCP tool. `args_schema` /
/// `response_schema` are opaque JSON Schema passed through verbatim to
/// the harness `wylde_describe`; the bridge does **not** validate model
/// arguments against them (the extension owns that, exactly as today).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct OperationDeclaration {
    #[serde(default)]
    pub description: String,
    /// The MCP tool (from this extension's `tools/list`) that fulfils
    /// this verb. May be empty for an `execute` op whose every action
    /// supplies its own `mcp_tool` override.
    #[serde(default)]
    pub mcp_tool: String,
    #[serde(default)]
    pub destructive: bool,
    /// Advisory tier label. Only `destructive` is enforced today
    /// (`destructive: true` → `destructive_tool_access` + consent);
    /// the string is reserved for a richer tier model (plan §6).
    #[serde(default = "default_tier")]
    pub tier: String,
    /// For `execute` ops: the legal `action` values, each backed by the
    /// op's `mcp_tool` or a per-action `mcp_tool` override.
    #[serde(default)]
    pub actions: Vec<ActionDeclaration>,
    #[serde(default)]
    pub args_schema: serde_json::Value,
    #[serde(default)]
    pub response_schema: serde_json::Value,
}

/// One legal `action` value of an `execute` op.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ActionDeclaration {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Per-action tool override; falls back to the op's `mcp_tool`.
    #[serde(default)]
    pub mcp_tool: Option<String>,
    #[serde(default)]
    pub destructive: bool,
}

fn default_scope() -> String { "global".to_string() }
fn default_schema_version() -> u32 { 1 }
fn default_tier() -> String { "read".to_string() }

/// How a UI panel is rendered inside the Wylde Tauri shell.
///
/// Only `iframe` is shipped today; the tagged-enum shape leaves room
/// for in-process renderers (`svelte_module`, `web_component`) without
/// breaking existing manifests.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelSource {
    /// Embed the panel as an `<iframe src=url>`. URL must be loopback
    /// (127.0.0.1, localhost, or ::1) — enforced at manifest load.
    Iframe { url: String },
}

/// One UI panel an extension surfaces inside the GUI's Tools tab.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UiPanel {
    /// Stable per-extension identifier; routes / nav state key off this.
    pub id: String,
    /// Display label in the nav.
    pub title: String,
    /// Optional emoji or short asset path. The GUI falls back to a
    /// default icon if absent or unrecognized.
    #[serde(default)]
    pub icon: Option<String>,
    /// How the panel is rendered.
    pub source: PanelSource,
}

/// The on-disk `mcp-server.json` contents.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Whether to spawn this extension's MCP server. Persisted across
    /// restarts by rewriting this file via `ext.enable` / `ext.disable`.
    #[serde(default)]
    pub enabled: bool,
    pub transport: Transport,
    /// Argv for the child process. Required for `stdio`.
    #[serde(default)]
    pub command: Vec<String>,
    /// Working directory the child runs in. If relative, resolved
    /// against the manifest's parent directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables for the child.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// For `http` transport: URL of the MCP server (deferred).
    #[serde(default)]
    pub url: Option<String>,
    /// Capabilities the extension declares (egress.web, ingress.browser,
    /// etc.). Read from legacy `manifest.json` if absent here.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Optional declarative tool catalog — lets the host return
    /// `ext.tools.list` for a *disabled* extension without spawning
    /// its server.
    #[serde(default)]
    pub tools: Vec<DeclaredTool>,
    /// UI panels the extension surfaces inside the GUI's Tools tab.
    /// Each panel's source URL is loopback-only (enforced at load).
    #[serde(default)]
    pub ui_panels: Vec<UiPanel>,
    /// Optional resource declarations (tool-registry consolidation
    /// Slice 5a). Absent ⇒ empty vec ⇒ today's named-tool behaviour.
    /// When present and verb mode is active, the listed `mcp_tool`s are
    /// *claimed* — hidden from the flat named-tool catalog and surfaced
    /// instead through the harness verb layer.
    #[serde(default)]
    pub resources: Vec<ResourceDeclaration>,
    #[serde(default)]
    pub health: HealthConfig,
}

impl McpServerManifest {
    /// Every MCP tool name claimed by a resource op (or per-action
    /// override). When verb mode is active these are subtracted from the
    /// aggregate named-tool catalog so a tool never appears in both the
    /// named surface and the resource surface (plan §5 partition rule).
    pub fn claimed_tools(&self) -> std::collections::BTreeSet<String> {
        let mut claimed = std::collections::BTreeSet::new();
        for res in &self.resources {
            claimed.extend(res.claimed_tools());
        }
        claimed
    }
}

fn default_version() -> String { "1.0".to_string() }

/// One extension's full state as loaded from disk: the
/// `mcp-server.json` manifest plus any legacy metadata harvested from
/// the sibling `manifest.json`.
#[derive(Debug, Clone)]
pub struct ExtensionRecord {
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub manifest: McpServerManifest,
    /// `browser_extension_path` from the legacy manifest, if any. Kept
    /// because Wylde_Study's MV3 chrome extension is shipped alongside
    /// the handler and the GUI surfaces this path.
    pub browser_extension_path: Option<PathBuf>,
    /// Capabilities merged from `mcp-server.json` + legacy
    /// `manifest.json`. If both declare them, mcp-server.json wins;
    /// otherwise the legacy manifest fills the gap.
    pub capabilities: Vec<String>,
    /// UI panels merged from `mcp-server.json` + legacy `manifest.json`.
    /// Same precedence as `capabilities`: mcp-server.json wins when
    /// both declare them, otherwise the legacy manifest fills the gap.
    pub ui_panels: Vec<UiPanel>,
}

impl ExtensionRecord {
    /// Resolve the child-process working directory.
    pub fn resolved_cwd(&self) -> PathBuf {
        match &self.manifest.cwd {
            Some(c) => {
                let p = PathBuf::from(c);
                if p.is_absolute() { p } else { self.root.join(p) }
            }
            None => self.root.clone(),
        }
    }
}

/// Load one extension by parsing `<root>/mcp-server.json` plus the
/// optional sibling `manifest.json`.
pub fn load_extension(root: &Path) -> Result<ExtensionRecord, ManifestError> {
    let manifest_path = root.join("mcp-server.json");
    if !manifest_path.exists() {
        return Err(ManifestError::NotFound(manifest_path));
    }

    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| ManifestError::Read { path: manifest_path.clone(), source: e })?;
    let manifest: McpServerManifest = serde_json::from_str(&raw)
        .map_err(|e| ManifestError::Parse { path: manifest_path.clone(), source: e })?;
    validate(&manifest_path, &manifest)?;

    let legacy = read_legacy(root);
    let mut capabilities = manifest.capabilities.clone();
    if capabilities.is_empty() {
        capabilities = legacy.capabilities;
    }
    let mut ui_panels = manifest.ui_panels.clone();
    if ui_panels.is_empty() {
        ui_panels = legacy.ui_panels;
    }
    // Loopback + uniqueness check applies to the resolved set so the
    // legacy harvest path can't smuggle a non-loopback panel past us.
    validate_ui_panels(&manifest_path, &ui_panels)?;

    Ok(ExtensionRecord {
        manifest_path,
        root: root.to_path_buf(),
        manifest,
        browser_extension_path: legacy.browser_extension_path,
        capabilities,
        ui_panels,
    })
}

fn validate(path: &Path, m: &McpServerManifest) -> Result<(), ManifestError> {
    if m.name.trim().is_empty() {
        return Err(ManifestError::Validation {
            path: path.to_path_buf(),
            message: "`name` must be a non-empty string".into(),
        });
    }
    match m.transport {
        Transport::Stdio => {
            if m.command.is_empty() {
                return Err(ManifestError::Validation {
                    path: path.to_path_buf(),
                    message: "stdio transport requires non-empty `command` argv".into(),
                });
            }
        }
        Transport::Http => {
            // Http transport is not implemented yet; reject manifests
            // that opt in so the failure is clear at load time rather
            // than at first dispatch.
            return Err(ManifestError::Validation {
                path: path.to_path_buf(),
                message: "http transport not yet supported by wylde-extension-bridge \
                         (Q-E4 — deferred). Use transport=stdio for now."
                    .into(),
            });
        }
    }
    let mut seen = std::collections::HashSet::new();
    for t in &m.tools {
        if t.tool_id.trim().is_empty() {
            return Err(ManifestError::Validation {
                path: path.to_path_buf(),
                message: "tool entry missing tool_id".into(),
            });
        }
        if !seen.insert(t.tool_id.clone()) {
            return Err(ManifestError::Validation {
                path: path.to_path_buf(),
                message: format!("duplicate tool_id `{}` within extension", t.tool_id),
            });
        }
    }
    validate_ui_panels(path, &m.ui_panels)?;
    validate_resources(path, m)?;
    Ok(())
}

/// Validate the `resources[]` block (plan §3.1). Hand-rolled, consistent
/// with how the rest of this module validates — no `jsonschema` crate.
/// The opaque `args_schema` / `response_schema` are intentionally **not**
/// validated; they are documentation for the LLM (R-VALID).
fn validate_resources(path: &Path, m: &McpServerManifest) -> Result<(), ManifestError> {
    let bail = |message: String| ManifestError::Validation {
        path: path.to_path_buf(),
        message,
    };
    // Static tool mirror for the load-time `mcp_tool` cross-check. Only
    // checked when non-empty; Rust extensions often omit it, deferring to
    // a runtime warn-once on first dispatch (R-RENAME).
    let tool_ids: std::collections::HashSet<&str> =
        m.tools.iter().map(|t| t.tool_id.as_str()).collect();
    let cross_check = !tool_ids.is_empty();

    let mut seen_types = std::collections::HashSet::new();
    for res in &m.resources {
        if res.resource_type.trim().is_empty() {
            return Err(bail("resource entry missing `resource_type`".into()));
        }
        if !seen_types.insert(res.resource_type.clone()) {
            return Err(bail(format!(
                "duplicate resource_type `{}` within extension",
                res.resource_type
            )));
        }
        if !matches!(res.scope.as_str(), "global" | "workspace" | "conversation") {
            return Err(bail(format!(
                "resource `{}` has invalid scope `{}` (expected global|workspace|conversation)",
                res.resource_type, res.scope
            )));
        }
        if res.schema_version != 1 {
            return Err(bail(format!(
                "resource `{}` declares schema_version {} — this bridge only \
                 understands version 1 (upgrade wylde-extension-bridge)",
                res.resource_type, res.schema_version
            )));
        }
        if res.operations.is_empty() {
            return Err(bail(format!(
                "resource `{}` declares no operations",
                res.resource_type
            )));
        }
        for (verb, op) in &res.operations {
            if !RESOURCE_VERBS.contains(&verb.as_str()) {
                return Err(bail(format!(
                    "resource `{}` has unknown operation verb `{}` (expected one of {:?})",
                    res.resource_type, verb, RESOURCE_VERBS
                )));
            }
            let is_execute = verb == "execute";
            // Every op needs *some* tool binding. Non-execute ops bind via
            // the op's own `mcp_tool`; an execute op may instead bind each
            // action through a per-action `mcp_tool` override.
            if op.mcp_tool.trim().is_empty() {
                let actions_cover = is_execute
                    && !op.actions.is_empty()
                    && op
                        .actions
                        .iter()
                        .all(|a| a.mcp_tool.as_deref().is_some_and(|t| !t.trim().is_empty()));
                if !actions_cover {
                    return Err(bail(format!(
                        "resource `{}` op `{}` has no `mcp_tool` (and no per-action \
                         override covering every action)",
                        res.resource_type, verb
                    )));
                }
            }
            // Action names: non-empty + unique within the op.
            let mut seen_actions = std::collections::HashSet::new();
            for act in &op.actions {
                if act.name.trim().is_empty() {
                    return Err(bail(format!(
                        "resource `{}` op `{}` has an action with an empty `name`",
                        res.resource_type, verb
                    )));
                }
                if !seen_actions.insert(act.name.clone()) {
                    return Err(bail(format!(
                        "resource `{}` op `{}` has duplicate action `{}`",
                        res.resource_type, verb, act.name
                    )));
                }
            }
            // Load-time cross-check against the static tools[] mirror.
            if cross_check {
                if !op.mcp_tool.is_empty() && !tool_ids.contains(op.mcp_tool.as_str()) {
                    return Err(bail(format!(
                        "resource `{}` op `{}` references mcp_tool `{}` not present in \
                         this extension's tools[] mirror",
                        res.resource_type, verb, op.mcp_tool
                    )));
                }
                for act in &op.actions {
                    if let Some(t) = act.mcp_tool.as_deref() {
                        if !t.is_empty() && !tool_ids.contains(t) {
                            return Err(bail(format!(
                                "resource `{}` op `{}` action `{}` references mcp_tool `{}` \
                                 not present in this extension's tools[] mirror",
                                res.resource_type, verb, act.name, t
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Validate the merged ui_panels list. The Tauri shell embeds these
/// URLs as `<iframe src>`, so anything pointing outside the local
/// machine would let a manifest exfiltrate session data to a remote
/// origin. We hard-refuse non-loopback URLs at load time so the GUI
/// never sees them.
fn validate_ui_panels(path: &Path, panels: &[UiPanel]) -> Result<(), ManifestError> {
    let mut seen = std::collections::HashSet::new();
    for p in panels {
        if p.id.trim().is_empty() {
            return Err(ManifestError::Validation {
                path: path.to_path_buf(),
                message: "ui_panel entry missing `id`".into(),
            });
        }
        if p.title.trim().is_empty() {
            return Err(ManifestError::Validation {
                path: path.to_path_buf(),
                message: format!("ui_panel `{}` missing `title`", p.id),
            });
        }
        if !seen.insert(p.id.clone()) {
            return Err(ManifestError::Validation {
                path: path.to_path_buf(),
                message: format!("duplicate ui_panel id `{}` within extension", p.id),
            });
        }
        match &p.source {
            PanelSource::Iframe { url } => {
                if !is_loopback_url(url) {
                    return Err(ManifestError::Validation {
                        path: path.to_path_buf(),
                        message: format!(
                            "ui_panel `{}` source url `{}` is not loopback — only \
                             http(s) URLs on 127.0.0.1, localhost, or [::1] are allowed",
                            p.id, url
                        ),
                    });
                }
            }
        }
    }
    Ok(())
}

/// True if `url` is an `http://` or `https://` URL whose host is the
/// loopback interface. We deliberately do this with string parsing
/// rather than pulling in the `url` crate; the loopback ruleset is
/// small and the failure mode (false positive) is conservative.
fn is_loopback_url(url: &str) -> bool {
    let rest = match url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        Some(r) => r,
        None => return false,
    };
    // Strip optional userinfo (`user:pass@host…`) — host starts after `@`.
    let after_userinfo = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    // Host ends at the first `/`, `?`, `#`, or end-of-string.
    let host_with_port = after_userinfo
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = if let Some(rest) = host_with_port.strip_prefix('[') {
        // IPv6 literal — `[host]:port` or `[host]`.
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        // IPv4 / DNS name — strip port if present.
        host_with_port.rsplit_once(':').map_or(host_with_port, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

struct LegacyHarvest {
    browser_extension_path: Option<PathBuf>,
    capabilities: Vec<String>,
    ui_panels: Vec<UiPanel>,
}

fn read_legacy(root: &Path) -> LegacyHarvest {
    let empty = LegacyHarvest {
        browser_extension_path: None,
        capabilities: Vec::new(),
        ui_panels: Vec::new(),
    };
    let legacy = root.join("manifest.json");
    if !legacy.exists() {
        return empty;
    }
    let Ok(raw) = std::fs::read_to_string(&legacy) else {
        return empty;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return empty;
    };
    let browser = json
        .get("browser_extension_path")
        .and_then(|v| v.as_str())
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() { p } else { root.join(p) }
        });
    let caps = json
        .get("capabilities")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let panels = json
        .get("ui_panels")
        .and_then(|v| serde_json::from_value::<Vec<UiPanel>>(v.clone()).ok())
        .unwrap_or_default();
    LegacyHarvest {
        browser_extension_path: browser,
        capabilities: caps,
        ui_panels: panels,
    }
}

/// Persist the `enabled` flag back to disk. Preserves field order /
/// formatting by round-tripping through a `serde_json::Value` rather
/// than re-emitting the typed struct.
pub fn write_enabled(path: &Path, enabled: bool) -> Result<(), ManifestError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ManifestError::Read { path: path.to_path_buf(), source: e })?;
    let mut json: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| ManifestError::Parse { path: path.to_path_buf(), source: e })?;
    if let Some(obj) = json.as_object_mut() {
        obj.insert("enabled".into(), serde_json::Value::Bool(enabled));
    } else {
        return Err(ManifestError::Validation {
            path: path.to_path_buf(),
            message: "mcp-server.json must be a JSON object".into(),
        });
    }
    let mut out = serde_json::to_string_pretty(&json)
        .map_err(|e| ManifestError::Parse { path: path.to_path_buf(), source: e })?;
    out.push('\n');
    std::fs::write(path, out)
        .map_err(|e| ManifestError::Read { path: path.to_path_buf(), source: e })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("mcp-server.json");
        std::fs::write(&path, contents).unwrap();
        root
    }

    #[test]
    fn parses_minimal_stdio_manifest() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "echo",
            r#"{"name":"echo","transport":"stdio","command":["echo","hello"]}"#,
        );
        let rec = load_extension(&root).expect("should parse");
        assert_eq!(rec.manifest.name, "echo");
        assert_eq!(rec.manifest.transport, Transport::Stdio);
        assert!(!rec.manifest.enabled);
        assert_eq!(rec.manifest.version, "1.0");
    }

    #[test]
    fn rejects_stdio_without_command() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "bad",
            r#"{"name":"bad","transport":"stdio"}"#,
        );
        assert!(matches!(load_extension(&root), Err(ManifestError::Validation { .. })));
    }

    #[test]
    fn rejects_http_for_now() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "httpext",
            r#"{"name":"httpext","transport":"http","url":"http://127.0.0.1:9000"}"#,
        );
        assert!(matches!(load_extension(&root), Err(ManifestError::Validation { .. })));
    }

    #[test]
    fn rejects_duplicate_tool_id() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "dups",
            r#"{"name":"dups","transport":"stdio","command":["x"],
                 "tools":[{"tool_id":"a"},{"tool_id":"a"}]}"#,
        );
        assert!(matches!(load_extension(&root), Err(ManifestError::Validation { .. })));
    }

    #[test]
    fn harvests_legacy_capabilities() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "legacy",
            r#"{"name":"legacy","transport":"stdio","command":["x"]}"#,
        );
        std::fs::write(
            root.join("manifest.json"),
            r#"{"name":"legacy","capabilities":["egress.web"],"browser_extension_path":"bx"}"#,
        )
        .unwrap();
        let rec = load_extension(&root).unwrap();
        assert_eq!(rec.capabilities, vec!["egress.web"]);
        assert!(rec.browser_extension_path.is_some());
    }

    #[test]
    fn round_trips_ui_panels_in_mcp_manifest() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "panels",
            r#"{
                "name":"panels",
                "transport":"stdio",
                "command":["x"],
                "ui_panels":[
                    {"id":"main","title":"Main","icon":"🧩",
                     "source":{"kind":"iframe","url":"http://127.0.0.1:5678"}},
                    {"id":"sub","title":"Sub",
                     "source":{"kind":"iframe","url":"http://localhost:5678/sub"}}
                ]
            }"#,
        );
        let rec = load_extension(&root).expect("should parse");
        assert_eq!(rec.ui_panels.len(), 2);
        assert_eq!(rec.ui_panels[0].id, "main");
        assert_eq!(rec.ui_panels[0].icon.as_deref(), Some("🧩"));
        assert!(matches!(
            rec.ui_panels[0].source,
            PanelSource::Iframe { ref url } if url == "http://127.0.0.1:5678"
        ));
        // Serialize back out and re-parse — schema is round-trip clean.
        let blob = serde_json::to_string(&rec.manifest).unwrap();
        let reparsed: McpServerManifest = serde_json::from_str(&blob).unwrap();
        assert_eq!(reparsed.ui_panels, rec.manifest.ui_panels);
    }

    #[test]
    fn rejects_non_loopback_panel_url() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "extpanel",
            r#"{
                "name":"extpanel",
                "transport":"stdio",
                "command":["x"],
                "ui_panels":[
                    {"id":"bad","title":"Bad",
                     "source":{"kind":"iframe","url":"http://evil.example.com/"}}
                ]
            }"#,
        );
        let err = load_extension(&root).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ManifestError::Validation { .. }), "expected validation error, got {err}");
        assert!(
            msg.contains("loopback"),
            "validation message should mention loopback, got: {msg}"
        );
    }

    #[test]
    fn rejects_duplicate_panel_ids() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "duppanels",
            r#"{
                "name":"duppanels",
                "transport":"stdio",
                "command":["x"],
                "ui_panels":[
                    {"id":"x","title":"A",
                     "source":{"kind":"iframe","url":"http://127.0.0.1:1"}},
                    {"id":"x","title":"B",
                     "source":{"kind":"iframe","url":"http://127.0.0.1:2"}}
                ]
            }"#,
        );
        assert!(matches!(
            load_extension(&root),
            Err(ManifestError::Validation { .. })
        ));
    }

    #[test]
    fn harvests_legacy_ui_panels() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "legpanels",
            r#"{"name":"legpanels","transport":"stdio","command":["x"]}"#,
        );
        std::fs::write(
            root.join("manifest.json"),
            r#"{
                "name":"legpanels",
                "ui_panels":[
                    {"id":"editor","title":"Editor",
                     "source":{"kind":"iframe","url":"http://127.0.0.1:5678"}}
                ]
            }"#,
        )
        .unwrap();
        let rec = load_extension(&root).unwrap();
        assert_eq!(rec.ui_panels.len(), 1);
        assert_eq!(rec.ui_panels[0].id, "editor");
    }

    #[test]
    fn mcp_panels_take_precedence_over_legacy_panels() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "precp",
            r#"{
                "name":"precp",
                "transport":"stdio",
                "command":["x"],
                "ui_panels":[
                    {"id":"win","title":"FromMcp",
                     "source":{"kind":"iframe","url":"http://127.0.0.1:1"}}
                ]
            }"#,
        );
        std::fs::write(
            root.join("manifest.json"),
            r#"{
                "name":"precp",
                "ui_panels":[
                    {"id":"loser","title":"FromLegacy",
                     "source":{"kind":"iframe","url":"http://127.0.0.1:2"}}
                ]
            }"#,
        )
        .unwrap();
        let rec = load_extension(&root).unwrap();
        assert_eq!(rec.ui_panels.len(), 1);
        assert_eq!(rec.ui_panels[0].title, "FromMcp");
    }

    #[test]
    fn legacy_non_loopback_panel_is_rejected() {
        // Defense-in-depth: even if a non-loopback URL slips into a
        // legacy manifest.json, the merged-set validator catches it.
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "legbad",
            r#"{"name":"legbad","transport":"stdio","command":["x"]}"#,
        );
        std::fs::write(
            root.join("manifest.json"),
            r#"{
                "name":"legbad",
                "ui_panels":[
                    {"id":"bad","title":"Bad",
                     "source":{"kind":"iframe","url":"https://attacker.example/"}}
                ]
            }"#,
        )
        .unwrap();
        assert!(matches!(
            load_extension(&root),
            Err(ManifestError::Validation { .. })
        ));
    }

    #[test]
    fn loopback_predicate_accepts_all_local_variants() {
        for url in [
            "http://127.0.0.1",
            "http://127.0.0.1/",
            "http://127.0.0.1:5678",
            "http://127.0.0.1:5678/path",
            "https://localhost:9000/x?q=1#frag",
            "http://[::1]:8080/",
            "http://user:pw@localhost:1/",
        ] {
            assert!(is_loopback_url(url), "expected loopback for {url}");
        }
        for url in [
            "http://example.com",
            "https://10.0.0.1",
            "ftp://localhost/",
            "file:///etc/hosts",
            "http://127.0.0.1.evil.com/",
            "//localhost/",
            "",
        ] {
            assert!(!is_loopback_url(url), "expected NOT loopback for {url}");
        }
    }

    // ── resources[] (Slice 5a) ───────────────────────────────────────

    const WEBCRAWLER_RESOURCES: &str = r#"{
        "name":"Webcrawler","transport":"stdio","command":["x"],
        "resources":[{
            "resource_type":"url",
            "display_name":"Web URL",
            "scope":"global",
            "schema_version":1,
            "operations":{
                "execute":{
                    "description":"web",
                    "destructive":false,
                    "tier":"read",
                    "actions":[
                        {"name":"fetch","mcp_tool":"fetch"},
                        {"name":"scrape","mcp_tool":"scrape"},
                        {"name":"extract","mcp_tool":"extract"}
                    ]
                }
            }
        }]
    }"#;

    #[test]
    fn parses_resources_block_and_computes_claimed_tools() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(td.path(), "wc", WEBCRAWLER_RESOURCES);
        let rec = load_extension(&root).expect("parses");
        assert_eq!(rec.manifest.resources.len(), 1);
        let res = &rec.manifest.resources[0];
        assert_eq!(res.resource_type, "url");
        assert_eq!(res.scope, "global");
        assert!(res.operations.contains_key("execute"));
        let claimed: Vec<String> = rec.manifest.claimed_tools().into_iter().collect();
        assert_eq!(claimed, vec!["extract", "fetch", "scrape"]); // BTreeSet → sorted
    }

    #[test]
    fn manifest_without_resources_has_empty_claimed_set() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "plain",
            r#"{"name":"plain","transport":"stdio","command":["x"]}"#,
        );
        let rec = load_extension(&root).unwrap();
        assert!(rec.manifest.resources.is_empty());
        assert!(rec.manifest.claimed_tools().is_empty());
    }

    #[test]
    fn rejects_invalid_resource_scope() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "badscope",
            r#"{"name":"badscope","transport":"stdio","command":["x"],
                "resources":[{"resource_type":"u","scope":"galaxy",
                    "operations":{"get":{"mcp_tool":"g"}}}]}"#,
        );
        let err = load_extension(&root).unwrap_err();
        assert!(err.to_string().contains("scope"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_operation_verb() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "badverb",
            r#"{"name":"badverb","transport":"stdio","command":["x"],
                "resources":[{"resource_type":"u",
                    "operations":{"frobnicate":{"mcp_tool":"g"}}}]}"#,
        );
        assert!(matches!(load_extension(&root), Err(ManifestError::Validation { .. })));
    }

    #[test]
    fn rejects_future_schema_version() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "newver",
            r#"{"name":"newver","transport":"stdio","command":["x"],
                "resources":[{"resource_type":"u","schema_version":2,
                    "operations":{"get":{"mcp_tool":"g"}}}]}"#,
        );
        let err = load_extension(&root).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "got: {err}");
    }

    #[test]
    fn rejects_op_with_no_tool_binding() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "notool",
            r#"{"name":"notool","transport":"stdio","command":["x"],
                "resources":[{"resource_type":"u",
                    "operations":{"get":{}}}]}"#,
        );
        let err = load_extension(&root).unwrap_err();
        assert!(err.to_string().contains("mcp_tool"), "got: {err}");
    }

    #[test]
    fn execute_with_per_action_overrides_needs_no_op_tool() {
        // The Webcrawler shape: execute op has no `mcp_tool`, but every
        // action supplies its own override — that's legal.
        let td = TempDir::new().unwrap();
        let root = write_manifest(td.path(), "exover", WEBCRAWLER_RESOURCES);
        assert!(load_extension(&root).is_ok());
    }

    #[test]
    fn rejects_duplicate_resource_type() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "duptype",
            r#"{"name":"duptype","transport":"stdio","command":["x"],
                "resources":[
                    {"resource_type":"u","operations":{"get":{"mcp_tool":"g"}}},
                    {"resource_type":"u","operations":{"get":{"mcp_tool":"h"}}}
                ]}"#,
        );
        assert!(matches!(load_extension(&root), Err(ManifestError::Validation { .. })));
    }

    #[test]
    fn cross_checks_mcp_tool_against_static_tools_mirror() {
        // When tools[] is present, a resource referencing a tool not in
        // the mirror is rejected at load.
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "xcheck",
            r#"{"name":"xcheck","transport":"stdio","command":["x"],
                "tools":[{"tool_id":"real"}],
                "resources":[{"resource_type":"u",
                    "operations":{"get":{"mcp_tool":"ghost"}}}]}"#,
        );
        let err = load_extension(&root).unwrap_err();
        assert!(err.to_string().contains("ghost"), "got: {err}");
    }

    #[test]
    fn write_enabled_flips_the_flag() {
        let td = TempDir::new().unwrap();
        let root = write_manifest(
            td.path(),
            "tog",
            r#"{"name":"tog","transport":"stdio","command":["x"],"enabled":false}"#,
        );
        let path = root.join("mcp-server.json");
        write_enabled(&path, true).unwrap();
        let rec = load_extension(&root).unwrap();
        assert!(rec.manifest.enabled);
    }
}
