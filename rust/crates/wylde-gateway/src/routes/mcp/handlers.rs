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
fn bridge_to_mcp(err: BridgeError) -> McpError {
    McpError {
        code: INTERNAL_ERROR,
        message: err.message,
        data: err.details,
    }
}

/// The `initialize` handshake reply. Capabilities are advertised as bare
/// objects — v1 supports listing/reading/calling but no `listChanged`
/// or `subscribe` notifications, so no sub-flags are set.
pub fn initialize() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
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

/// Route one JSON-RPC method to its handler and return the `result`.
///
/// Returns `Err(McpError)` for any failure the transport must render as
/// a JSON-RPC `error`.
pub async fn dispatch(method: &str, params: &Value) -> Result<Value, McpError> {
    match method {
        "initialize" => Ok(initialize()),
        "tools/list" => {
            let tools = adapters::list_tools().await.map_err(bridge_to_mcp)?;
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = require_str(params, "name")?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return Err(McpError::new(
                    INVALID_PARAMS,
                    "'arguments' must be an object",
                ));
            }
            adapters::call_tool(&name, arguments)
                .await
                .map_err(bridge_to_mcp)
        }
        "resources/list" => {
            let resources = adapters::list_resources().await.map_err(bridge_to_mcp)?;
            Ok(json!({ "resources": resources }))
        }
        "resources/read" => {
            let uri = require_str(params, "uri")?;
            adapters::read_resource(&uri).await.map_err(bridge_to_mcp)
        }
        "prompts/list" => {
            let prompts = adapters::list_prompts().await.map_err(bridge_to_mcp)?;
            Ok(json!({ "prompts": prompts }))
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

    #[test]
    fn initialize_advertises_protocol_and_capabilities() {
        let result = initialize();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["serverInfo"]["version"], SERVER_VERSION);
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
        assert!(result["capabilities"]["prompts"].is_object());
    }

    #[tokio::test]
    async fn dispatch_initialize_needs_no_pipe() {
        let result = dispatch("initialize", &json!({})).await.unwrap();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn dispatch_unknown_method_is_method_not_found() {
        let err = dispatch("does/not/exist", &json!({})).await.unwrap_err();
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_notification_is_accepted_as_empty_result() {
        let result = dispatch("notifications/initialized", &json!({}))
            .await
            .unwrap();
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn dispatch_tools_call_rejects_missing_name() {
        let err = dispatch("tools/call", &json!({})).await.unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn dispatch_tools_call_rejects_non_object_arguments() {
        let err = dispatch("tools/call", &json!({ "name": "t", "arguments": 5 }))
            .await
            .unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn dispatch_resources_read_rejects_missing_uri() {
        let err = dispatch("resources/read", &json!({})).await.unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn dispatch_tools_list_bridges_to_unreachable_harness_as_internal_error() {
        // No harness pipe in the unit-test sandbox — the bridge failure
        // must surface as a JSON-RPC internal error, not a panic.
        let err = dispatch("tools/list", &json!({})).await.unwrap_err();
        assert_eq!(err.code, INTERNAL_ERROR);
    }
}
