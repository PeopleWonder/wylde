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
//! | `resources/list`  | `conversations.list` + `rag.workspaces.list`|
//! | `resources/read`  | `conversations.get` \| workspace file store |
//! | `prompts/list`    | `prompts.list` (catalog entries)            |
//! | `prompts/get`     | `prompts.list` (override-or-default resolve)|
//!
//! The harness pipe actions are NOT modified — this is a read/run
//! surface layered on top of them.

use serde_json::{json, Value};

use crate::proxy_core::pipe_action;

/// Harness pipe service name — every action dispatches here.
pub const HARNESS_PIPE: &str = "wylde-harness";
/// URI scheme for the Wylde resource namespace.
pub const URI_SCHEME: &str = "wylde://";

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
async fn harness(action: &str, payload: Value) -> Result<Value, BridgeError> {
    match pipe_action(HARNESS_PIPE, action, payload).await {
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
                .unwrap_or_else(|| format!("harness action {action:?} failed"));
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
fn entries(data: &Value, key: &str) -> Vec<Value> {
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

/// Return the harness tool catalog in MCP `Tool` shape.
pub async fn list_tools() -> Result<Value, BridgeError> {
    let data = harness("tools.list", json!({})).await?;
    let tools: Vec<Value> = entries(&data, "tools").iter().map(tool_to_mcp).collect();
    Ok(Value::Array(tools))
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
    json!({ "name": name, "description": description, "inputSchema": schema })
}

/// Run one tool through the harness `tools.run` action.
///
/// `tools.run` calls `tool_runner.run_tool(name, args, confirm=…)` — the
/// same dispatch path an in-process turn uses. The runner envelope is
/// serialised into a single MCP text-content block; `isError` mirrors
/// the envelope's `ok` flag.
pub async fn call_tool(name: &str, arguments: Value) -> Result<Value, BridgeError> {
    let reply = harness(
        "tools.run",
        json!({ "name": name, "args": arguments, "confirm": false }),
    )
    .await?;
    Ok(tool_result_to_mcp(&reply))
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

// ── resources ──────────────────────────────────────────────────────────

/// Enumerate readable resources: recent conversations + workspaces.
///
/// Conversations are listed first, then workspaces — the Python side
/// keeps the same order so a bridge failure surfaces identically.
pub async fn list_resources() -> Result<Value, BridgeError> {
    let mut out: Vec<Value> = Vec::new();
    let convs = harness("conversations.list", json!({})).await?;
    for conv in entries(&convs, "conversations") {
        let cid = conv.get("id").and_then(Value::as_str).unwrap_or("");
        if cid.is_empty() {
            continue;
        }
        let name = conv
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(cid);
        out.push(json!({
            "uri": format!("{URI_SCHEME}conversation/{cid}"),
            "name": name,
            "mimeType": "application/json",
        }));
    }
    let wss = harness("rag.workspaces.list", json!({})).await?;
    for ws in entries(&wss, "workspaces") {
        let wid = ws.get("id").and_then(Value::as_str).unwrap_or("");
        if wid.is_empty() {
            continue;
        }
        let name = ws
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(wid);
        out.push(json!({
            "uri": format!("{URI_SCHEME}workspace/{wid}/"),
            "name": name,
            "mimeType": "inode/directory",
        }));
    }
    Ok(Value::Array(out))
}

/// A parsed `wylde://` resource URI.
#[derive(Debug, PartialEq, Eq)]
pub enum ResourceRef {
    Conversation(String),
    Workspace { id: String, path: String },
}

/// Split a `wylde://` URI into a [`ResourceRef`].
///
/// * `wylde://conversation/<id>`             → `Conversation(id)`
/// * `wylde://workspace/<workspace_id>/<p>`  → `Workspace { id, path }`
///
/// Returns `None` for any other shape.
pub fn parse_uri(uri: &str) -> Option<ResourceRef> {
    let rest = uri.strip_prefix(URI_SCHEME)?;
    let mut parts = rest.splitn(3, '/');
    match parts.next()? {
        "conversation" => {
            let id = parts.next().filter(|s| !s.is_empty())?;
            Some(ResourceRef::Conversation(id.to_owned()))
        }
        "workspace" => {
            let id = parts.next().filter(|s| !s.is_empty())?;
            let path = parts.next().unwrap_or("");
            Some(ResourceRef::Workspace {
                id: id.to_owned(),
                path: path.to_owned(),
            })
        }
        _ => None,
    }
}

/// Resolve a `wylde://` URI to its MCP `contents` block.
pub async fn read_resource(uri: &str) -> Result<Value, BridgeError> {
    match parse_uri(uri) {
        None => Err(BridgeError::msg(format!(
            "unsupported resource uri: {uri:?}"
        ))),
        Some(ResourceRef::Conversation(id)) => {
            let doc = harness("conversations.get", json!({ "id": id })).await?;
            let text = serde_json::to_string(&doc).unwrap_or_else(|_| "null".to_owned());
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text,
                }]
            }))
        }
        Some(ResourceRef::Workspace { id, path }) => {
            if path.is_empty() {
                return Err(BridgeError::msg(format!(
                    "workspace resource uri needs a file path: {uri:?}"
                )));
            }
            let text = read_workspace_file(&id, &path).await?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": text,
                }]
            }))
        }
    }
}

/// Read a file under a workspace's indexed folder. The workspace root
/// comes from the `rag.workspaces.list` registry.
async fn read_workspace_file(workspace_id: &str, rel_path: &str) -> Result<String, BridgeError> {
    let wss = harness("rag.workspaces.list", json!({})).await?;
    let workspace = entries(&wss, "workspaces")
        .into_iter()
        .find(|w| w.get("id").and_then(Value::as_str) == Some(workspace_id))
        .ok_or_else(|| BridgeError::msg(format!("workspace not found: {workspace_id:?}")))?;
    let root = workspace.get("path").and_then(Value::as_str).unwrap_or("");
    if root.is_empty() {
        return Err(BridgeError::msg(format!(
            "workspace {workspace_id:?} has no indexed path"
        )));
    }
    resolve_and_read(root, rel_path)
}

/// Resolve `rel_path` against `root`, confine it to the workspace, and
/// read it. A `../` that escapes the workspace root is rejected.
pub fn resolve_and_read(root: &str, rel_path: &str) -> Result<String, BridgeError> {
    let base = std::fs::canonicalize(root)
        .map_err(|exc| BridgeError::msg(format!("workspace root unavailable: {exc}")))?;
    let target = std::fs::canonicalize(base.join(rel_path))
        .map_err(|_| BridgeError::msg(format!("file not found in workspace: {rel_path:?}")))?;
    if !target.starts_with(&base) {
        return Err(BridgeError::msg(format!(
            "path escapes workspace root: {rel_path:?}"
        )));
    }
    if !target.is_file() {
        return Err(BridgeError::msg(format!(
            "file not found in workspace: {rel_path:?}"
        )));
    }
    std::fs::read_to_string(&target)
        .map_err(|exc| BridgeError::msg(format!("could not read workspace file: {exc}")))
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
        assert_eq!(mapped["inputSchema"]["properties"]["path"]["type"], "string");
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
    fn parse_uri_handles_conversation() {
        assert_eq!(
            parse_uri("wylde://conversation/abc-123"),
            Some(ResourceRef::Conversation("abc-123".to_owned()))
        );
    }

    #[test]
    fn parse_uri_handles_workspace_with_nested_path() {
        assert_eq!(
            parse_uri("wylde://workspace/ws-9/src/main.rs"),
            Some(ResourceRef::Workspace {
                id: "ws-9".to_owned(),
                path: "src/main.rs".to_owned(),
            })
        );
    }

    #[test]
    fn parse_uri_workspace_root_has_empty_path() {
        assert_eq!(
            parse_uri("wylde://workspace/ws-9/"),
            Some(ResourceRef::Workspace {
                id: "ws-9".to_owned(),
                path: String::new(),
            })
        );
    }

    #[test]
    fn parse_uri_rejects_foreign_scheme_and_unknown_kind() {
        assert_eq!(parse_uri("https://example.test/x"), None);
        assert_eq!(parse_uri("wylde://memory/abc"), None);
        assert_eq!(parse_uri("wylde://conversation/"), None);
        assert_eq!(parse_uri("wylde://"), None);
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

    #[test]
    fn resolve_and_read_reads_a_file_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello workspace").unwrap();
        let root = dir.path().to_str().unwrap();
        assert_eq!(resolve_and_read(root, "note.txt").unwrap(), "hello workspace");
    }

    #[test]
    fn resolve_and_read_rejects_traversal_outside_the_workspace() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("secret.txt"), "top secret").unwrap();
        let inner = outer.path().join("workspace");
        std::fs::create_dir(&inner).unwrap();
        let root = inner.to_str().unwrap();
        // `../secret.txt` resolves outside the workspace root.
        let err = resolve_and_read(root, "../secret.txt").unwrap_err();
        assert!(
            err.message.contains("escapes") || err.message.contains("not found"),
            "unexpected message: {}",
            err.message
        );
    }
}
