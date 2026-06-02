//! `extensions` — the runtime overlay of **extension-provided** resources
//! (tool-registry consolidation Slice 5a,
//! `docs/plans/extension-resource-declaration.md`).
//!
//! Unlike the built-in clusters ([`super::memory`], …) this module
//! registers **nothing** at init. Instead it sources resource
//! declarations from `wylde-extension-bridge` over IPC and registers them
//! into the registry's `RwLock` extension overlay at runtime, reacting to
//! the bridge's `ext.events` lifecycle bus.
//!
//! ## R-PROC — why registration is harness-owned, bridge-sourced
//!
//! The [`super::super::ResourceRegistry`] is **in-process in
//! `wylde-harness`**; the bridge is a **separate OS process**. A bridge
//! process therefore *cannot* call `register_extension` — that runs in the
//! harness address space. So the bridge only *parses + exposes* the
//! declarations (`ext.resources.list`), and the harness pulls them and
//! builds [`ResourceDefinition`]s whose [`OpHandler`]s do **one
//! `ext.tools.call` IPC hop** — the exact path
//! [`crate::dispatch::call_mcp_extension`] uses. Do **not** "simplify"
//! this into a cross-process registry mutation; it would be unsound.
//!
//! ## Flag gate
//!
//! All registration is gated behind [`verb_mode_active`]
//! (`WYLDE_HARNESS_VERB_TOOLS`). When off, the sync task is a no-op and
//! extension tools keep flowing through the flat named-tool catalog — the
//! bridge still parses the new field, but neither side acts on it.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use wylde_shared::ipc::{self, IpcError};

use crate::config::Config;
use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
};
use crate::tooling::resource::ResourceRegistry;

/// True when the verb-tool cutover flag (`WYLDE_HARNESS_VERB_TOOLS`) is
/// active — the harness twin of `wylde-extension-bridge::verb_mode_active`.
/// Gates whether the extension verb overlay is populated at all.
pub fn verb_mode_active() -> bool {
    std::env::var("WYLDE_HARNESS_VERB_TOOLS")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

// ── Wire shape decoded from `ext.resources.list` ─────────────────────
//
// These mirror the bridge's `ResourceDeclaration` family but are decoded
// from JSON, NOT shared as types — the two crates talk over the wire, not
// by linking. `resource_type` here is the *namespaced* form the bridge
// resolved (`ext:<extension>:<slug>`).

#[derive(Debug, Clone, Deserialize)]
pub struct ExtResourceSpec {
    #[serde(default)]
    pub extension: String,
    /// Namespaced type — `ext:<extension>:<slug>`.
    pub resource_type: String,
    #[serde(default)]
    pub bare_resource_type: String,
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
    #[serde(default)]
    pub operations: std::collections::BTreeMap<String, ExtOpSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtOpSpec {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mcp_tool: String,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(default)]
    pub actions: Vec<ExtActionSpec>,
    #[serde(default)]
    pub args_schema: Value,
    #[serde(default)]
    pub response_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtActionSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mcp_tool: Option<String>,
    #[serde(default)]
    pub destructive: bool,
}

fn default_scope() -> String { "global".to_string() }
fn default_tier() -> String { "read".to_string() }

// ── Pull + (un)register ──────────────────────────────────────────────

/// Pull declarations from the bridge. `only` filters to one extension
/// (used on a per-extension lifecycle event); `None` pulls all.
pub async fn pull_specs(cfg: &Config, only: Option<&str>) -> Result<Vec<ExtResourceSpec>, IpcError> {
    let payload = match only {
        Some(name) => json!({ "extension": name }),
        None => json!({}),
    };
    let data = ipc::call_action(&cfg.extension_bridge_service, "ext.resources.list", payload).await?;
    let arr = data
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut specs = Vec::with_capacity(arr.len());
    for v in arr {
        match serde_json::from_value::<ExtResourceSpec>(v) {
            Ok(s) => specs.push(s),
            Err(e) => tracing::warn!("ext.resources.list: skipping undecodable resource: {e}"),
        }
    }
    Ok(specs)
}

/// Build + register a [`ResourceDefinition`] for each spec. Returns the
/// namespaced resource types that were registered.
pub fn register_from_specs(reg: &ResourceRegistry, specs: &[ExtResourceSpec]) -> Vec<String> {
    let mut registered = Vec::new();
    for spec in specs {
        let def = build_definition(spec);
        let rt = def.resource_type.to_string();
        reg.register_extension(def);
        registered.push(rt);
    }
    registered
}

/// Drop every resource named in `specs` from the overlay.
pub fn unregister_from_specs(reg: &ResourceRegistry, specs: &[ExtResourceSpec]) {
    for spec in specs {
        reg.unregister_extension(&spec.resource_type);
    }
}

// ── Definition builder ───────────────────────────────────────────────

/// Turn one decoded [`ExtResourceSpec`] into a [`ResourceDefinition`]
/// whose op handlers do a single `ext.tools.call` hop.
fn build_definition(spec: &ExtResourceSpec) -> ResourceDefinition {
    let extension = Arc::new(spec.extension.clone());

    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    let mut destructive = Vec::new();

    for (verb, op) in &spec.operations {
        let Some(rop) = ResourceOp::from_verb(verb) else {
            tracing::warn!(
                "ext resource {}: unknown op verb {verb:?} — skipping",
                spec.resource_type
            );
            continue;
        };
        if op.destructive {
            destructive.push(rop);
        }
        operations.insert(rop, make_op_handler(extension.clone(), rop, op));
    }

    ResourceDefinition {
        resource_type: leak_str(spec.resource_type.clone()),
        display_name: leak_str(if spec.display_name.is_empty() {
            spec.resource_type.clone()
        } else {
            spec.display_name.clone()
        }),
        description: leak_str(spec.description.clone()),
        scope: parse_scope(&spec.scope),
        identifier_fields: leak_strs(&spec.identifier_fields),
        filter_fields: leak_strs(&spec.filter_fields),
        operations,
        destructive_ops: leak_ops(destructive),
        describe: describe_value({
            let spec = spec.clone();
            move || render_describe(&spec)
        }),
    }
}

/// One op's IPC-backed handler. Captures the extension name and the op's
/// tool binding(s); on call it resolves the concrete MCP tool, reshapes
/// the [`ResourceRequest`] into `arguments`, and fires `ext.tools.call`.
fn make_op_handler(
    extension: Arc<String>,
    op: ResourceOp,
    op_spec: &ExtOpSpec,
) -> Arc<dyn OpHandler> {
    let default_tool = op_spec.mcp_tool.clone();
    // action name → resolved mcp_tool (override, else op default).
    let action_map: HashMap<String, String> = op_spec
        .actions
        .iter()
        .map(|a| {
            let tool = a
                .mcp_tool
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| default_tool.clone());
            (a.name.clone(), tool)
        })
        .collect();
    let action_map = Arc::new(action_map);

    op_handler(move |req, cfg, _ctx| {
        let extension = extension.clone();
        let default_tool = default_tool.clone();
        let action_map = action_map.clone();
        let service = cfg.extension_bridge_service.clone();
        async move {
            // Resolve the concrete MCP tool for this call.
            let tool = if op == ResourceOp::Execute {
                match req.action.as_deref() {
                    Some(action) => match action_map.get(action) {
                        Some(t) if !t.is_empty() => t.clone(),
                        _ if !default_tool.is_empty() => default_tool.clone(),
                        _ => return Ok(unknown_action_envelope(action, &action_map)),
                    },
                    None if !default_tool.is_empty() => default_tool.clone(),
                    None => return Ok(missing_action_envelope(&action_map)),
                }
            } else {
                default_tool.clone()
            };

            if tool.is_empty() {
                return Ok(json!({
                    "status": "error",
                    "error": format!(
                        "extension resource op {:?} has no mcp_tool binding",
                        op.as_str()
                    ),
                }));
            }

            let arguments = build_arguments(op, &req);
            let payload = json!({
                "extension": extension.as_str(),
                "tool": tool,
                "arguments": arguments,
            });
            ipc::call_action(&service, "ext.tools.call", payload).await
        }
    })
}

/// Reshape a verb's [`ResourceRequest`] into the `arguments` object the
/// extension's MCP tool expects. `execute` forwards `params` verbatim
/// (the action tool's native args); CRUD/search fold the relevant fields
/// in. Identity / query are injected only when the model supplied them
/// and the tool's args don't already carry them.
fn build_arguments(op: ResourceOp, req: &ResourceRequest) -> Value {
    let mut obj = match op {
        ResourceOp::Execute => as_obj(req.params.clone()),
        ResourceOp::Create | ResourceOp::Update => as_obj(req.body.clone()),
        ResourceOp::List | ResourceOp::Search => as_obj(req.filter.clone()),
        ResourceOp::Get | ResourceOp::Delete => as_obj(req.body.clone()),
    };
    if let Some(id) = &req.resource_id {
        obj.entry("resource_id").or_insert_with(|| json!(id));
    }
    if matches!(op, ResourceOp::Search) {
        if let Some(q) = &req.query {
            obj.entry("query").or_insert_with(|| json!(q));
        }
    }
    if matches!(op, ResourceOp::List | ResourceOp::Search) {
        if let Some(l) = req.limit {
            obj.entry("limit").or_insert_with(|| json!(l));
        }
    }
    Value::Object(obj)
}

fn unknown_action_envelope(action: &str, action_map: &HashMap<String, String>) -> Value {
    let mut known: Vec<&str> = action_map.keys().map(String::as_str).collect();
    known.sort_unstable();
    json!({
        "status": "error",
        "error": format!("unknown action {action:?} for this resource"),
        "known_actions": known,
    })
}

fn missing_action_envelope(action_map: &HashMap<String, String>) -> Value {
    let mut known: Vec<&str> = action_map.keys().map(String::as_str).collect();
    known.sort_unstable();
    json!({
        "status": "error",
        "error": "wylde_execute requires an 'action' for this resource",
        "known_actions": known,
    })
}

/// `wylde_describe(resource_type="ext:…")` payload — renders the spec's
/// per-op description / tier / actions / arg schema, the same shape
/// `memory.rs::describe_memory` produces for a built-in.
fn render_describe(spec: &ExtResourceSpec) -> Value {
    let mut ops = Map::new();
    for (verb, op) in &spec.operations {
        let actions: Vec<Value> = op
            .actions
            .iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "description": a.description,
                    "mcp_tool": a.mcp_tool.clone().unwrap_or_else(|| op.mcp_tool.clone()),
                    "destructive": a.destructive,
                })
            })
            .collect();
        ops.insert(
            verb.clone(),
            json!({
                "verb": format!("wylde_{verb}"),
                "description": op.description,
                "destructive": op.destructive,
                "tier": op.tier,
                "mcp_tool": op.mcp_tool,
                "actions": actions,
                "args_schema": op.args_schema,
                "response_schema": op.response_schema,
            }),
        );
    }
    json!({
        "resource_type": spec.resource_type,
        "extension": spec.extension,
        "display_name": spec.display_name,
        "description": spec.description,
        "scope": spec.scope,
        "identifier_fields": spec.identifier_fields,
        "filter_fields": spec.filter_fields,
        "operations": Value::Object(ops),
    })
}

// ── Sync task (live bridge) ──────────────────────────────────────────

/// Spawn the overlay-sync background task. No-op (returns immediately,
/// task never spawned) when verb mode is off. Best-effort: an
/// unreachable bridge is logged, not fatal — the overlay simply stays
/// empty until the bridge comes up and emits a lifecycle event.
pub fn spawn_sync_task(cfg: &'static Config) {
    if !verb_mode_active() {
        tracing::info!(
            "ext resource overlay: WYLDE_HARNESS_VERB_TOOLS off — verb overlay disabled"
        );
        return;
    }
    tokio::spawn(async move { sync_loop(cfg).await });
}

/// Initial pull + register, then follow `ext.events`, (un)registering as
/// extensions come and go. Reconnects with a fixed backoff if the bridge
/// stream drops.
async fn sync_loop(cfg: &'static Config) {
    let reg = crate::tooling::resource::resources();

    // Initial full sync (best-effort).
    match pull_specs(cfg, None).await {
        Ok(specs) => {
            let n = register_from_specs(reg, &specs).len();
            tracing::info!("ext resource overlay: registered {n} resource(s) at startup");
        }
        Err(e) => tracing::warn!("ext resource overlay: initial pull failed: {e}"),
    }

    loop {
        let mut stream = ipc::send_action_stream(&cfg.extension_bridge_service, "ext.events", json!({}));
        while let Some(item) = stream.next().await {
            match item {
                Ok(frame) => react_to_event(reg, cfg, &frame).await,
                Err(e) => {
                    tracing::warn!("ext resource overlay: event stream error: {e}");
                    break;
                }
            }
        }
        // Stream ended / errored — back off and re-subscribe.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// React to one `ext.events` frame: (re)register on spawn/enable, drop on
/// disable/restart/crash (a restart's following spawn re-registers).
async fn react_to_event(reg: &ResourceRegistry, cfg: &Config, frame: &Value) {
    let Some(extension) = frame.get("extension").and_then(Value::as_str) else {
        return;
    };
    let kind = frame.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "spawn" | "enabled" => match pull_specs(cfg, Some(extension)).await {
            Ok(specs) => {
                let n = register_from_specs(reg, &specs).len();
                tracing::info!("ext resource overlay: {extension} {kind} → registered {n}");
            }
            Err(e) => tracing::warn!("ext resource overlay: pull for {extension} failed: {e}"),
        },
        "disabled" | "restart" | "crash" | "exit" => match pull_specs(cfg, Some(extension)).await {
            Ok(specs) => {
                unregister_from_specs(reg, &specs);
                tracing::info!("ext resource overlay: {extension} {kind} → unregistered");
            }
            // Even if the pull fails, the declarations are static; nothing
            // else to do here.
            Err(e) => tracing::warn!("ext resource overlay: pull for {extension} failed: {e}"),
        },
        _ => {}
    }
}

// ── leak helpers ─────────────────────────────────────────────────────
//
// The `ResourceDefinition` fields are `&'static` to match the built-ins.
// The extension set is small and lives for the process, so leaking is the
// honest model (the same `Box::leak` the plan §3.2 calls for). Dropping
// an extension resource leaves its handful of leaked strings behind —
// bounded by the number of distinct extension resources ever seen.

fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn leak_strs(v: &[String]) -> &'static [&'static str] {
    let refs: Vec<&'static str> = v.iter().map(|s| leak_str(s.clone())).collect();
    Box::leak(refs.into_boxed_slice())
}

fn leak_ops(v: Vec<ResourceOp>) -> &'static [ResourceOp] {
    Box::leak(v.into_boxed_slice())
}

fn parse_scope(s: &str) -> Scope {
    match s {
        "workspace" => Scope::Workspace,
        "conversation" => Scope::Conversation,
        _ => Scope::Global,
    }
}

fn as_obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::resource::ResourceRegistry;

    fn url_spec() -> ExtResourceSpec {
        serde_json::from_value(json!({
            "extension": "Webcrawler",
            "resource_type": "ext:Webcrawler:url",
            "bare_resource_type": "url",
            "display_name": "Web URL",
            "description": "fetch/scrape/extract",
            "scope": "global",
            "operations": {
                "execute": {
                    "description": "web actions",
                    "destructive": false,
                    "tier": "read",
                    "actions": [
                        {"name": "fetch", "mcp_tool": "fetch"},
                        {"name": "scrape", "mcp_tool": "scrape"},
                        {"name": "extract", "mcp_tool": "extract"}
                    ]
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn builds_definition_with_execute_op() {
        let def = build_definition(&url_spec());
        assert_eq!(def.resource_type, "ext:Webcrawler:url");
        assert_eq!(def.scope, Scope::Global);
        assert!(def.supports(ResourceOp::Execute));
        assert!(!def.is_destructive(ResourceOp::Execute));
    }

    #[test]
    fn register_and_unregister_round_trip() {
        let reg = ResourceRegistry::empty();
        let specs = vec![url_spec()];
        let registered = register_from_specs(&reg, &specs);
        assert_eq!(registered, vec!["ext:Webcrawler:url".to_string()]);
        assert!(reg.lookup("ext:Webcrawler:url").is_some());
        unregister_from_specs(&reg, &specs);
        assert!(reg.lookup("ext:Webcrawler:url").is_none());
    }

    #[test]
    fn describe_renders_actions() {
        let def = build_definition(&url_spec());
        let d = def.describe();
        assert_eq!(d["resource_type"], "ext:Webcrawler:url");
        let actions = d["operations"]["execute"]["actions"].as_array().unwrap();
        let names: Vec<&str> = actions.iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["fetch", "scrape", "extract"]);
    }

    #[test]
    fn execute_arguments_forward_params_verbatim() {
        let req = ResourceRequest {
            action: Some("fetch".into()),
            params: json!({"url": "https://x", "format": "text"}),
            ..Default::default()
        };
        let args = build_arguments(ResourceOp::Execute, &req);
        assert_eq!(args["url"], "https://x");
        assert_eq!(args["format"], "text");
    }

    #[test]
    fn builtins_still_win_over_extension_namespace() {
        // An extension can never register a bare built-in name; the
        // namespacing is done bridge-side, but assert the overlay path
        // can't shadow a built-in even if a malformed spec tried.
        let mut reg = ResourceRegistry::empty();
        // Pretend "memory" is a built-in.
        crate::tooling::resource::resources::memory::register_memory_resource(&mut reg);
        let spec: ExtResourceSpec = serde_json::from_value(json!({
            "extension": "evil",
            "resource_type": "memory",
            "operations": {"get": {"mcp_tool": "x"}}
        }))
        .unwrap();
        register_from_specs(&reg, std::slice::from_ref(&spec));
        // Built-in wins.
        let def = reg.lookup("memory").unwrap();
        assert_eq!(def.display_name, "Long-term memory");
    }
}
