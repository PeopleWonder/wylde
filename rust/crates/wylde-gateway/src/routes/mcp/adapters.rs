//! Adapters — bridge MCP method shapes to the harness pipe actions.
//!
//! Rust port of `Gateway/routes/mcp/adapters.py`. Every MCP capability
//! is a thin reshape over an existing `\\.\pipe\wylde-harness` action;
//! this module owns that reshape so [`super::handlers`] stays a pure
//! JSON-RPC dispatcher. Each function mirrors its Python counterpart
//! action-for-action and shape-for-shape — the parity gate in
//! `rust/tests/parity/tests/gateway.rs` holds the two together.
//!
//! MCP spec: <https://spec.modelcontextprotocol.io/> (revision 2025-06-18).
//!
//! ## Action map
//!
//! | MCP method        | Harness action(s)                          |
//! |-------------------|--------------------------------------------|
//! | `tools/list`      | `tools.list`                               |
//! | `tools/call`      | `tools.run` (runs `tool_runner.run_tool`)   |
//! | `resources/list`  | `conversations.list` + `workspaces.list_mru` |
//! | `resources/read`  | `conversations.get` \| workspace file store |
//! | `prompts/list`    | `prompts.list` (catalog entries)            |
//! | `prompts/get`     | `prompts.list` (override-or-default resolve)|
//!
//! The harness pipe actions are NOT modified — this is a read/run
//! surface layered on top of them.
//!
//! Tool-exposure policy lives in [`super::authz`]; the `resources/*`
//! reshape lives in [`super::resources`]. This module keeps the shared
//! harness bridge ([`harness`], [`entries`], [`BridgeError`]) plus the
//! tools and prompts reshapes.

use serde_json::{json, Value};

use super::authz::{
    classify_entry, entry_destructive, is_exposable, tier_allows_destructive, ToolAccess,
};
use crate::proxy_core::pipe_action;

/// Harness pipe service name — every action dispatches here.
pub const HARNESS_PIPE: &str = "wylde-harness";
/// URI scheme for the Wylde resource namespace.
pub const URI_SCHEME: &str = "wylde://";

/// Classify how `name` may be reached over MCP by consulting the live
/// catalog. Only called for a privileged (`destructive_tool_access`)
/// caller asking for a non-allow-listed tool, so the extra `tools.list`
/// round-trip is paid only on that path.
pub async fn classify_tool(name: &str) -> Result<ToolAccess, BridgeError> {
    let data = harness("tools.list", json!({})).await?;
    let all = entries(&data, "tools");
    let entry = all.iter().find(|e| entry_name(e) == name);
    Ok(classify_entry(entry, name))
}

/// A harness pipe action failed. Carries a human-readable `message` and
/// optional structured `details`; [`super::handlers`] folds it into a
/// JSON-RPC error.
#[derive(Debug)]
pub struct BridgeError {
    pub message: String,
    pub details: Option<Value>,
}

impl BridgeError {
    /// Build a detail-less bridge error.
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: None,
        }
    }
}

/// Invoke a harness pipe action and return its reply `data`.
pub(super) async fn harness(action: &str, payload: Value) -> Result<Value, BridgeError> {
    call(HARNESS_PIPE, action, payload).await
}

/// Invoke `action` on the `service` pipe and return its reply `data`.
pub(super) async fn call(
    service: &str,
    action: &str,
    payload: Value,
) -> Result<Value, BridgeError> {
    match pipe_action(service, action, payload).await {
        Ok(data) => Ok(data),
        Err((status, body)) => {
            let code = body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("bridge_error")
                .to_owned();
            let message = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{service} action {action:?} failed"));
            Err(BridgeError {
                message,
                details: Some(json!({
                    "action": action,
                    "code": code,
                    "status": status.as_u16(),
                })),
            })
        }
    }
}

/// Pull a list of object entries out of a harness reply that is either
/// `{<key>: [...]}`, `{<key>: {...}}`, or a bare list.
pub(super) fn entries(data: &Value, key: &str) -> Vec<Value> {
    if let Some(inner) = data.get(key) {
        return match inner {
            Value::Array(a) => a.iter().filter(|v| v.is_object()).cloned().collect(),
            Value::Object(m) => m.values().filter(|v| v.is_object()).cloned().collect(),
            _ => Vec::new(),
        };
    }
    match data {
        Value::Array(a) => a.iter().filter(|v| v.is_object()).cloned().collect(),
        _ => Vec::new(),
    }
}

// ── tools ──────────────────────────────────────────────────────────────

/// Return the harness tool catalog in MCP `Tool` shape, scoped to what the
/// caller's `device_tier` may reach. Non-destructive allow-listed tools are
/// always listed; destructive tools are listed **only** for a
/// `destructive_tool_access` caller, and carry `annotations.destructiveHint`
/// so the client knows a `confirm` is required. `tools/list` and
/// `tools/call` therefore agree on what a given caller may run.
pub async fn list_tools(device_tier: &str) -> Result<Value, BridgeError> {
    let data = harness("tools.list", json!({})).await?;
    let allow_destructive = tier_allows_destructive(device_tier);
    let tools: Vec<Value> = entries(&data, "tools")
        .iter()
        .filter(|e| entry_listable(e, allow_destructive))
        .map(tool_to_mcp)
        .collect();
    Ok(Value::Array(tools))
}

/// The canonical name for a catalog entry — `name`, else `id`. Kept in
/// step with [`tool_to_mcp`] so the exposure filter and the emitted
/// `Tool.name` always agree.
fn entry_name(entry: &Value) -> &str {
    entry
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| entry.get("id").and_then(Value::as_str))
        .unwrap_or("")
}

/// Whether a catalog entry is listable for a caller: a destructive tool is
/// listed only when `allow_destructive`; a non-destructive tool is listed
/// only when allow-listed. Pure.
fn entry_listable(entry: &Value, allow_destructive: bool) -> bool {
    if entry_destructive(entry) {
        allow_destructive
    } else {
        is_exposable(entry_name(entry))
    }
}

/// Map one canonical harness catalog entry to an MCP `Tool`.
pub fn tool_to_mcp(entry: &Value) -> Value {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| entry.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();
    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let schema = ["input_schema", "inputSchema", "parameters", "schema"]
        .iter()
        .find_map(|k| entry.get(*k))
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    // MCP tool annotations (spec `ToolAnnotations`): advertise the
    // destructive/read-only hints so a client knows which tools need a
    // `confirm`. Derived from the harness `destructive` flag, the source
    // of truth.
    let destructive = entry_destructive(entry);
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": { "readOnlyHint": !destructive, "destructiveHint": destructive },
    })
}

/// Run one tool through the harness `tools.run` action.
///
/// `tools.run`'s contract is `{name, args?, device_tier?}` and it runs the
/// registry **tier gate** against `device_tier` (Rust port note in
/// `pipe/tools.rs`). We pass the *caller's* real tier — resolved by
/// `require_device` — so a `tool_use` device is held to `tool_use` and
/// only a `destructive_tool_access` device can reach a destructive tool,
/// exactly as an interactive turn would be gated. An empty tier lets the
/// harness apply its `tool_use` default.
///
/// (The old `confirm: false` field was a no-op — it is not part of the
/// `tools.run` contract — and is dropped; destructive gating is the tier
/// gate's job, not a client-supplied flag.)
///
/// The runner envelope is serialised into a single MCP text-content
/// block; `isError` mirrors the envelope's `ok` flag.
pub async fn call_tool(
    name: &str,
    arguments: Value,
    device_tier: &str,
    confirm: bool,
) -> Result<Value, BridgeError> {
    let reply = harness(
        "tools.run",
        run_payload(name, arguments, device_tier, confirm),
    )
    .await?;
    Ok(tool_result_to_mcp(&reply))
}

/// Build the `tools.run` payload: `{name, args, device_tier?, confirm?}`.
/// The tier is included only when non-empty (an empty tier lets the
/// harness apply its `tool_use` default). `confirm` is included only when
/// true — it is a per-call, non-persisted confirmation that satisfies an
/// undecided harness consent gate for this dispatch (it never overrides a
/// stored deny; that check is the harness's).
fn run_payload(name: &str, arguments: Value, device_tier: &str, confirm: bool) -> Value {
    let mut payload = json!({ "name": name, "args": arguments });
    if !device_tier.is_empty() {
        payload["device_tier"] = json!(device_tier);
    }
    if confirm {
        payload["confirm"] = json!(true);
    }
    payload
}

/// Wrap a `tool_runner` envelope into an MCP `CallToolResult`.
pub fn tool_result_to_mcp(reply: &Value) -> Value {
    let is_error = !reply.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let text = serde_json::to_string(reply).unwrap_or_else(|_| "null".to_owned());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

// ── prompts ────────────────────────────────────────────────────────────

/// Return the prompt catalog in MCP `Prompt` shape (name + description).
pub async fn list_prompts() -> Result<Value, BridgeError> {
    let data = harness("prompts.list", json!({})).await?;
    let catalog = data
        .get("catalog")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prompts: Vec<Value> = catalog.iter().filter_map(prompt_to_mcp).collect();
    Ok(Value::Array(prompts))
}

/// Map one prompt-catalog entry to an MCP `Prompt`. Entries with no
/// usable id are dropped (`None`).
pub fn prompt_to_mcp(entry: &Value) -> Option<Value> {
    let id = entry.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    Some(json!({ "name": id, "description": prompt_desc(entry) }))
}

fn prompt_desc(entry: &Value) -> String {
    entry
        .get("desc")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| entry.get("label").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

/// Resolve one prompt's active text into an MCP `GetPromptResult`.
pub async fn get_prompt(name: &str) -> Result<Value, BridgeError> {
    let data = harness("prompts.list", json!({})).await?;
    resolve_prompt(&data, name)
}

/// Resolve a prompt by id from a `prompts.list` reply — the saved
/// override if present, otherwise the catalog default.
pub fn resolve_prompt(data: &Value, name: &str) -> Result<Value, BridgeError> {
    let catalog = data
        .get("catalog")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entry = catalog
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(name))
        .ok_or_else(|| BridgeError::msg(format!("unknown prompt: {name:?}")))?;
    let override_text = data
        .get("overrides")
        .and_then(|o| o.get(name))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let default_text = entry.get("default").and_then(Value::as_str).unwrap_or("");
    let text = override_text.unwrap_or(default_text);
    Ok(json!({
        "description": prompt_desc(entry),
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": text },
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_listable_scopes_destructive_tools_to_the_privileged_tier() {
        let safe = json!({ "id": "read_file", "destructive": false });
        let danger = json!({ "id": "write_file", "destructive": true });
        // Non-destructive allow-listed tool: listed for everyone.
        assert!(entry_listable(&safe, false));
        assert!(entry_listable(&safe, true));
        // Destructive tool: listed only when destructive is allowed.
        assert!(!entry_listable(&danger, false));
        assert!(entry_listable(&danger, true));
        // Non-destructive but NOT allow-listed: never listed.
        let hidden = json!({ "id": "execute_bash", "destructive": false });
        assert!(!entry_listable(&hidden, false));
        assert!(!entry_listable(&hidden, true));
    }

    #[test]
    fn tool_to_mcp_annotates_destructive_hint() {
        let safe = tool_to_mcp(&json!({ "id": "read_file", "destructive": false }));
        assert_eq!(safe["annotations"]["destructiveHint"], false);
        assert_eq!(safe["annotations"]["readOnlyHint"], true);
        let danger = tool_to_mcp(&json!({ "id": "write_file", "destructive": true }));
        assert_eq!(danger["annotations"]["destructiveHint"], true);
        assert_eq!(danger["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn tool_to_mcp_prefers_name_then_id() {
        let entry = json!({ "id": "git_status", "name": "git.status", "description": "d" });
        let mapped = tool_to_mcp(&entry);
        assert_eq!(mapped["name"], "git.status");
        assert_eq!(mapped["description"], "d");
    }

    #[test]
    fn tool_to_mcp_falls_back_to_id_and_default_schema() {
        let entry = json!({ "id": "git_status" });
        let mapped = tool_to_mcp(&entry);
        assert_eq!(mapped["name"], "git_status");
        assert_eq!(mapped["inputSchema"], json!({ "type": "object" }));
    }

    #[test]
    fn tool_to_mcp_keeps_declared_schema() {
        let schema = json!({ "type": "object", "properties": { "path": {"type": "string"} } });
        let entry = json!({ "id": "t", "parameters": schema });
        let mapped = tool_to_mcp(&entry);
        assert_eq!(
            mapped["inputSchema"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn run_payload_threads_tier_and_confirm() {
        let p = run_payload("read_file", json!({ "path": "a.txt" }), "tool_use", true);
        assert_eq!(p["name"], "read_file");
        assert_eq!(p["device_tier"], "tool_use");
        assert_eq!(
            p["confirm"], true,
            "confirm:true must be forwarded to tools.run"
        );
        // confirm:false is omitted (the harness default), so it never
        // appears in args and stays off unless explicitly set.
        let p2 = run_payload("write_file", json!({}), "destructive_tool_access", false);
        assert_eq!(p2["device_tier"], "destructive_tool_access");
        assert!(p2.get("confirm").is_none(), "confirm:false is omitted");
        // Empty tier is omitted so the harness applies its default.
        let p3 = run_payload("read_file", json!({}), "", true);
        assert!(p3.get("device_tier").is_none());
        assert_eq!(p3["confirm"], true);
    }

    #[test]
    fn tool_result_marks_error_when_envelope_not_ok() {
        let ok = tool_result_to_mcp(&json!({ "ok": true, "data": 1 }));
        assert_eq!(ok["isError"], false);
        let bad = tool_result_to_mcp(&json!({ "ok": false, "error": {"code": "x"} }));
        assert_eq!(bad["isError"], true);
        assert_eq!(bad["content"][0]["type"], "text");
    }

    #[test]
    fn prompt_to_mcp_drops_entry_without_id() {
        assert!(prompt_to_mcp(&json!({ "label": "x" })).is_none());
        assert!(prompt_to_mcp(&json!({ "id": "" })).is_none());
        let ok = prompt_to_mcp(&json!({ "id": "core", "desc": "Core prompt" })).unwrap();
        assert_eq!(ok["name"], "core");
        assert_eq!(ok["description"], "Core prompt");
    }

    #[test]
    fn resolve_prompt_prefers_override_over_default() {
        let data = json!({
            "catalog": [{ "id": "core", "label": "Core", "desc": "d", "default": "DEFAULT" }],
            "overrides": { "core": "OVERRIDDEN" },
        });
        let result = resolve_prompt(&data, "core").unwrap();
        assert_eq!(result["messages"][0]["content"]["text"], "OVERRIDDEN");
        assert_eq!(result["description"], "d");
    }

    #[test]
    fn resolve_prompt_uses_default_when_no_override() {
        let data = json!({
            "catalog": [{ "id": "core", "label": "Core", "desc": "", "default": "DEFAULT" }],
            "overrides": {},
        });
        let result = resolve_prompt(&data, "core").unwrap();
        assert_eq!(result["messages"][0]["content"]["text"], "DEFAULT");
        // Empty desc falls back to label.
        assert_eq!(result["description"], "Core");
    }

    #[test]
    fn resolve_prompt_unknown_id_is_bridge_error() {
        let data = json!({ "catalog": [], "overrides": {} });
        assert!(resolve_prompt(&data, "missing").is_err());
    }

    #[test]
    fn entries_handles_keyed_list_dict_and_bare_list() {
        let keyed = json!({ "tools": [{ "id": "a" }, { "id": "b" }] });
        assert_eq!(entries(&keyed, "tools").len(), 2);
        let keyed_dict = json!({ "tools": { "a": { "id": "a" } } });
        assert_eq!(entries(&keyed_dict, "tools").len(), 1);
        let bare = json!([{ "id": "a" }]);
        assert_eq!(entries(&bare, "tools").len(), 1);
    }
}
