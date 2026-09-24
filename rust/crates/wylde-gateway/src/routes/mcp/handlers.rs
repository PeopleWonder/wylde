//! JSON-RPC method dispatch for the MCP surface.
//!
//! Rust port of `Gateway/routes/mcp/handlers.py`. Maps each MCP method
//! onto an adapter call:
//!
//! * `initialize`      — protocol handshake; advertises capabilities.
//! * `tools/list`      — [`adapters::list_tools`].
//! * `tools/call`      — [`adapters::call_tool`].
//! * `resources/list`  — [`adapters::list_resources`].
//! * `resources/read`  — [`adapters::read_resource`].
//! * `prompts/list`    — [`adapters::list_prompts`].
//! * `prompts/get`     — [`adapters::get_prompt`].
//! * `notifications/*` — accepted, no-op (v1 acts on no client
//!   notifications).
//!
//! Anything else is JSON-RPC `-32601` (method not found). Deliberately
//! out of scope for v1: `sampling`, `*/subscribe`, `.../list_changed`
//! notifications, `completion/complete`, `logging/setLevel`, `roots`.
//!
//! MCP spec: <https://spec.modelcontextprotocol.io/> (revision 2025-06-18).

use serde_json::{json, Value};

use super::adapters::{self, BridgeError};
use crate::auth::Device;

// ── Protocol identity ──────────────────────────────────────────────────

/// Pinned to the current stable MCP revision. Bump deliberately — a
/// protocol change is a surface change the parity gate must re-verify.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// Server identity reported in the `initialize` handshake.
pub const SERVER_NAME: &str = "wylde-gateway-mcp";
pub const SERVER_VERSION: &str = "1.0.0";

// ── JSON-RPC 2.0 error codes ───────────────────────────────────────────

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

// ── Server-defined errors (JSON-RPC reserves -32000..=-32099) ───────────

/// A `tools/call` for a tool that is not exposed to this caller — an
/// unknown/hidden tool, or a destructive tool the caller's tier may not
/// run. Distinct from `METHOD_NOT_FOUND` (about JSON-RPC *methods*).
pub const TOOL_NOT_PERMITTED: i64 = -32001;

/// A `tools/call` for a destructive tool the caller's tier *may* run, but
/// without the explicit `confirm: true` argument. The client should
/// resend with confirmation — it must never be run silently.
pub const CONFIRMATION_REQUIRED: i64 = -32002;

/// A JSON-RPC error the transport serialises into an `error` response.
#[derive(Debug)]
pub struct McpError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl McpError {
    /// Build a data-less JSON-RPC error.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Fold a [`BridgeError`] (the harness pipe is down / returned an error)
/// into a JSON-RPC internal error.
///
/// The structured `details` (internal harness action name, harness error
/// code, pipe HTTP status) are **not** forwarded to the client — they are
/// logged server-side instead, so an external MCP caller learns only a
/// human-readable message, not the gateway's internal topology (M3).
fn bridge_to_mcp(err: BridgeError) -> McpError {
    if let Some(details) = err.details.as_ref() {
        tracing::warn!(target: "mcp", message = %err.message, details = %details, "mcp bridge error");
    }
    McpError {
        code: INTERNAL_ERROR,
        message: err.message,
        data: None,
    }
}

/// Page size for the paginated `*/list` methods. A client that passes no
/// cursor gets the first page plus a `nextCursor` when more remain.
const PAGE_SIZE: usize = 100;

/// Apply opaque-cursor pagination to a full result list. The cursor is a
/// server-defined opaque token (a decimal offset); a client MUST echo it
/// verbatim. Returns the page plus the `nextCursor` for the following
/// page, or `None` when the list is exhausted.
fn paginate(
    items: Vec<Value>,
    cursor: Option<&str>,
) -> Result<(Vec<Value>, Option<String>), McpError> {
    let start = match cursor {
        None => 0,
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| McpError::new(INVALID_PARAMS, "invalid pagination cursor"))?,
    };
    if start >= items.len() {
        return Ok((Vec::new(), None));
    }
    let end = (start + PAGE_SIZE).min(items.len());
    let next = (end < items.len()).then(|| end.to_string());
    Ok((items[start..end].to_vec(), next))
}

/// Build a paginated `*/list` result: `{<key>: [page], nextCursor?}`.
fn list_result(key: &str, items: Vec<Value>, cursor: Option<&str>) -> Result<Value, McpError> {
    let (page, next) = paginate(items, cursor)?;
    let mut out = json!({ key: page });
    if let Some(next) = next {
        out["nextCursor"] = json!(next);
    }
    Ok(out)
}

/// Pull the `cursor` param, if any.
fn cursor_of(params: &Value) -> Option<&str> {
    params.get("cursor").and_then(Value::as_str)
}

/// Coerce an adapter list reply (`Value::Array`) into a `Vec<Value>`.
fn as_items(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a,
        _ => Vec::new(),
    }
}

/// Protocol revisions this server can speak. Currently exactly one; the
/// list is the negotiation surface so adding a revision is a one-line
/// change here.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[MCP_PROTOCOL_VERSION];

/// The `initialize` handshake reply. Negotiates the protocol version:
/// if the client's requested `protocolVersion` is one we support, we echo
/// it; otherwise we answer with our latest and let the client decide
/// whether to proceed (per the MCP handshake). Capabilities are advertised
/// as bare objects — v1 supports listing/reading/calling but no
/// `listChanged` or `subscribe` notifications, so no sub-flags are set.
pub fn initialize(params: &Value) -> Value {
    let negotiated = match params.get("protocolVersion").and_then(Value::as_str) {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        _ => MCP_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": negotiated,
        "capabilities": {
            "tools": {},
            "resources": {},
            "prompts": {},
        },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
    })
}

fn require_str(params: &Value, key: &str) -> Result<String, McpError> {
    match params.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_owned()),
        _ => Err(McpError::new(
            INVALID_PARAMS,
            format!("missing or invalid {key:?} parameter"),
        )),
    }
}

/// Decide whether a classified `tools/call` may proceed. Pure policy over
/// the resolved access class + the client's `confirm` flag:
///
/// * `Allowed` → run.
/// * `Destructive` + `confirm` → run.
/// * `Destructive` without `confirm` → `CONFIRMATION_REQUIRED`.
/// * `NotExposed` → `TOOL_NOT_PERMITTED`.
fn decide(access: &adapters::ToolAccess, name: &str, confirm: bool) -> Result<(), McpError> {
    match access {
        adapters::ToolAccess::Allowed => Ok(()),
        adapters::ToolAccess::Destructive if confirm => Ok(()),
        adapters::ToolAccess::Destructive => Err(McpError::new(
            CONFIRMATION_REQUIRED,
            format!("tool {name:?} is destructive; resend the call with arguments.confirm = true to run it"),
        )),
        adapters::ToolAccess::NotExposed => Err(McpError::new(
            TOOL_NOT_PERMITTED,
            format!("tool {name:?} is not exposed over MCP"),
        )),
    }
}

/// Route one JSON-RPC method to its handler and return the `result`.
///
/// `device` is the caller verified by `require_device`; its tier decides
/// what `tools/call` may run (threaded down to the harness tier gate).
///
/// Returns `Err(McpError)` for any failure the transport must render as
/// a JSON-RPC `error`.
pub async fn dispatch(device: &Device, method: &str, params: &Value) -> Result<Value, McpError> {
    match method {
        "initialize" => Ok(initialize(params)),
        "tools/list" => {
            let tools = adapters::list_tools(&device.tier)
                .await
                .map_err(bridge_to_mcp)?;
            list_result("tools", as_items(tools), cursor_of(params))
        }
        "tools/call" => {
            let name = require_str(params, "name")?;
            let mut arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return Err(McpError::new(
                    INVALID_PARAMS,
                    "'arguments' must be an object",
                ));
            }
            // `confirm` is an MCP protocol flag, not a tool argument —
            // read it, then strip it so it never reaches the tool.
            let confirm = arguments
                .get("confirm")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(obj) = arguments.as_object_mut() {
                obj.remove("confirm");
            }

            // Fast path: an allow-listed tool is known non-destructive and
            // pre-vetted for MCP, so it runs with no extra catalog
            // round-trip. We send confirm:true so a first-time call clears
            // the harness's undecided consent gate rather than returning
            // consent_required — a stored deny still wins downstream.
            if adapters::is_exposable(&name) {
                return adapters::call_tool(&name, arguments, &device.tier, true)
                    .await
                    .map_err(bridge_to_mcp);
            }

            // Off the allow-list. A non-privileged tier can run nothing
            // here, so refuse without a catalog round-trip (and without
            // leaking whether the tool exists). Only a
            // destructive_tool_access caller pays the classify lookup.
            let access = if adapters::tier_allows_destructive(&device.tier) {
                adapters::classify_tool(&name)
                    .await
                    .map_err(bridge_to_mcp)?
            } else {
                adapters::ToolAccess::NotExposed
            };
            decide(&access, &name, confirm)?;
            // Reached only for a destructive tool the caller confirmed
            // (decide guarantees confirm == true here) — forward the
            // confirmation so the harness runs it end-to-end.
            adapters::call_tool(&name, arguments, &device.tier, confirm)
                .await
                .map_err(bridge_to_mcp)
        }
        "resources/list" => {
            let resources = adapters::list_resources().await.map_err(bridge_to_mcp)?;
            list_result("resources", as_items(resources), cursor_of(params))
        }
        "resources/read" => {
            let uri = require_str(params, "uri")?;
            adapters::read_resource(&uri).await.map_err(bridge_to_mcp)
        }
        "prompts/list" => {
            let prompts = adapters::list_prompts().await.map_err(bridge_to_mcp)?;
            list_result("prompts", as_items(prompts), cursor_of(params))
        }
        "prompts/get" => {
            let name = require_str(params, "name")?;
            adapters::get_prompt(&name).await.map_err(bridge_to_mcp)
        }
        // Client-to-server notifications need no action in v1 — accept
        // silently. The transport drops the (empty) result for a true
        // notification; a stray request gets `{}`.
        m if m.starts_with("notifications/") => Ok(json!({})),
        other => Err(McpError::new(
            METHOD_NOT_FOUND,
            format!("method not found: {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `tool_use`-tier caller for the dispatch tests.
    fn device() -> Device {
        Device {
            device_id: "dev-test".to_owned(),
            tier: "tool_use".to_owned(),
        }
    }

    #[test]
    fn initialize_advertises_protocol_and_capabilities() {
        let result = initialize(&json!({}));
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["serverInfo"]["version"], SERVER_VERSION);
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
        assert!(result["capabilities"]["prompts"].is_object());
    }

    #[test]
    fn initialize_echoes_a_supported_requested_version() {
        let result = initialize(&json!({ "protocolVersion": MCP_PROTOCOL_VERSION }));
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_falls_back_on_an_unsupported_version() {
        let result = initialize(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[test]
    fn paginate_emits_next_cursor_only_when_more_remain() {
        let items: Vec<Value> = (0..PAGE_SIZE + 5).map(|i| json!(i)).collect();
        let (page, next) = paginate(items.clone(), None).unwrap();
        assert_eq!(page.len(), PAGE_SIZE);
        assert_eq!(next.as_deref(), Some(PAGE_SIZE.to_string().as_str()));
        // Second page: the remainder, no further cursor.
        let (page2, next2) = paginate(items, next.as_deref()).unwrap();
        assert_eq!(page2.len(), 5);
        assert!(next2.is_none());
    }

    #[test]
    fn paginate_rejects_a_garbage_cursor() {
        let err = paginate(vec![json!(1)], Some("not-a-number")).unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[test]
    fn paginate_past_the_end_is_an_empty_final_page() {
        let (page, next) = paginate(vec![json!(1), json!(2)], Some("99")).unwrap();
        assert!(page.is_empty());
        assert!(next.is_none());
    }

    #[tokio::test]
    async fn dispatch_initialize_needs_no_pipe() {
        let result = dispatch(&device(), "initialize", &json!({})).await.unwrap();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn dispatch_unknown_method_is_method_not_found() {
        let err = dispatch(&device(), "does/not/exist", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_notification_is_accepted_as_empty_result() {
        let result = dispatch(&device(), "notifications/initialized", &json!({}))
            .await
            .unwrap();
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn dispatch_tools_call_rejects_missing_name() {
        let err = dispatch(&device(), "tools/call", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn dispatch_tools_call_refuses_non_allowlisted_tool() {
        // A tool that exists in the harness but is NOT on the MCP
        // allow-list, called by a non-privileged (tool_use) device, must be
        // refused before the pipe is touched — the authorization gate.
        let err = dispatch(
            &device(),
            "tools/call",
            &json!({ "name": "write_file", "arguments": { "path": "/x", "content": "y" } }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, TOOL_NOT_PERMITTED);
    }

    #[tokio::test]
    async fn dispatch_tools_call_tier_wins_over_confirm() {
        // A non-privileged device cannot run an off-list (destructive)
        // tool even if it sets confirm:true — refused pre-pipe, tier wins.
        let err = dispatch(
            &device(),
            "tools/call",
            &json!({ "name": "write_file", "arguments": { "confirm": true, "path": "/x" } }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, TOOL_NOT_PERMITTED);
    }

    #[test]
    fn decide_matrix() {
        use adapters::ToolAccess::*;
        // Non-destructive allow-listed → run.
        assert!(decide(&Allowed, "read_file", false).is_ok());
        // Destructive + confirm → run.
        assert!(decide(&Destructive, "write_file", true).is_ok());
        // Destructive without confirm → confirmation required.
        assert_eq!(
            decide(&Destructive, "write_file", false).unwrap_err().code,
            CONFIRMATION_REQUIRED
        );
        // Not exposed → authorization refusal (confirm is irrelevant).
        assert_eq!(
            decide(&NotExposed, "secret", true).unwrap_err().code,
            TOOL_NOT_PERMITTED
        );
    }

    #[tokio::test]
    async fn dispatch_tools_call_rejects_non_object_arguments() {
        // Use an allow-listed name so the check reached is the argument
        // shape, not the allow-list gate.
        let err = dispatch(
            &device(),
            "tools/call",
            &json!({ "name": "read_file", "arguments": 5 }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn dispatch_resources_read_rejects_missing_uri() {
        let err = dispatch(&device(), "resources/read", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn dispatch_tools_list_bridges_to_unreachable_harness_as_internal_error() {
        // No harness pipe in the unit-test sandbox — the bridge failure
        // must surface as a JSON-RPC internal error, not a panic.
        let err = dispatch(&device(), "tools/list", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, INTERNAL_ERROR);
    }
}
