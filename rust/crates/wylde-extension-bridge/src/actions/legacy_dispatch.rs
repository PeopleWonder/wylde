//! `extensions.dispatch` back-compat alias.
//!
//! The Python `wylde-extension-bridge` exposed exactly one pipe action,
//! `extensions.dispatch`, with payload `{extension, endpoint, params}`.
//! The Gateway today (`rust/crates/wylde-gateway/src/routes/extensions.rs`)
//! is wired against that exact shape — when this Rust impl runs in
//! place of Python under the strangler env var, that wire MUST keep
//! working byte-identically until the Gateway is updated.
//!
//! Mapping: `{extension, endpoint, params}` -> `ext.tools.call`
//! with `tool = endpoint`. Error codes get translated to the legacy
//! shape `extension_not_found` / `extension_disabled` / `extension_error`
//! so Gateway's `map_bridge_code` keeps producing the same HTTP statuses.

use std::sync::Arc;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};

use crate::host::Host;
use crate::mcp::McpError;

pub async fn handle_extensions_dispatch(host: Arc<Host>, payload: Value) -> Reply {
    let obj = match &payload {
        Value::Object(m) => m,
        _ => return Reply::err(IpcError::new("bad_request", "payload must be a map")),
    };
    let extension = obj
        .get("extension")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let endpoint = obj
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = obj.get("params").cloned().unwrap_or(json!({}));

    if extension.is_empty() || endpoint.is_empty() {
        return Reply::err(IpcError::new(
            "bad_request",
            "`extension` and `endpoint` are required strings",
        ));
    }

    // Map per the host's status before dispatch so we can return the
    // legacy error codes Gateway expects.
    match host.get_status(&extension).await {
        None => {
            return Reply::err(IpcError::new(
                "extension_not_found",
                format!("no extension `{extension}`"),
            ));
        }
        Some(s) if !s.enabled => {
            return Reply::err(IpcError::new(
                "extension_disabled",
                format!("extension `{extension}` is disabled"),
            ));
        }
        Some(_) => {}
    }

    match host.call_tool(&extension, &endpoint, params).await {
        Ok(v) => Reply::ok(v),
        Err(e) => {
            let (code, msg) = match &e {
                McpError::Server { code, message } => (
                    "extension_error",
                    format!("mcp_server_error code={code} {message}"),
                ),
                McpError::Transport(m) => ("extension_error", m.clone()),
                McpError::InitTimeout(_) | McpError::CallTimeout(_) => {
                    ("extension_error", e.to_string())
                }
                McpError::UnsupportedSpecVersion { .. } => ("extension_error", e.to_string()),
                McpError::Spawn(_) => ("extension_error", e.to_string()),
                McpError::Decode(_) => ("extension_error", e.to_string()),
            };
            Reply::err(IpcError::new(code, msg))
        }
    }
}
