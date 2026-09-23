//! Implementation of the nine first-class extension actions plus
//! `ext.events` streaming.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::host::{Host, LifecycleEvent};
use crate::mcp::McpError;

/// Map an [`McpError`] to a wire-friendly [`IpcError`] code.
fn err_code(e: &McpError) -> &'static str {
    match e {
        McpError::Spawn(_) => "extension_spawn_failed",
        McpError::Transport(_) => "extension_transport_error",
        McpError::InitTimeout(_) => "extension_init_timeout",
        McpError::CallTimeout(_) => "extension_call_timeout",
        McpError::UnsupportedSpecVersion { .. } => "mcp_spec_version_unsupported",
        McpError::Server { .. } => "mcp_server_error",
        McpError::Decode(_) => "mcp_decode_error",
    }
}

fn ipc_err(e: McpError) -> IpcError {
    IpcError::new(err_code(&e), e.to_string())
}

// ────────────────────────────────────────────────────────────────────
// 1. ext.list
// ────────────────────────────────────────────────────────────────────

pub async fn handle_list(host: Arc<Host>, _payload: Value) -> Reply {
    let items = host.list_status().await;
    Reply::ok(json!({ "extensions": items }))
}

// ────────────────────────────────────────────────────────────────────
// 2. ext.get
// ────────────────────────────────────────────────────────────────────

pub async fn handle_get(host: Arc<Host>, payload: Value) -> Reply {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return Reply::err(IpcError::new("bad_request", "`name` (string) required"));
    };
    match host.get_status(name).await {
        Some(s) => Reply::ok(serde_json::to_value(s).unwrap_or(json!({}))),
        None => Reply::err(IpcError::new(
            "extension_not_found",
            format!("no extension `{name}`"),
        )),
    }
}

// ────────────────────────────────────────────────────────────────────
// 3. ext.enable
// ────────────────────────────────────────────────────────────────────

pub async fn handle_enable(host: Arc<Host>, payload: Value) -> Reply {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return Reply::err(IpcError::new("bad_request", "`name` (string) required"));
    };
    match host.set_enabled(name, true).await {
        Ok(s) => Reply::ok(serde_json::to_value(s).unwrap_or(json!({}))),
        Err(e) => Reply::err(ipc_err(e)),
    }
}

// ────────────────────────────────────────────────────────────────────
// 4. ext.disable
// ────────────────────────────────────────────────────────────────────

pub async fn handle_disable(host: Arc<Host>, payload: Value) -> Reply {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return Reply::err(IpcError::new("bad_request", "`name` (string) required"));
    };
    match host.set_enabled(name, false).await {
        Ok(s) => Reply::ok(serde_json::to_value(s).unwrap_or(json!({}))),
        Err(e) => Reply::err(ipc_err(e)),
    }
}

// ────────────────────────────────────────────────────────────────────
// 5. ext.tools.list — aggregate or per-extension
// ────────────────────────────────────────────────────────────────────

pub async fn handle_tools_list(host: Arc<Host>, payload: Value) -> Reply {
    match payload.get("extension").and_then(Value::as_str) {
        Some(name) => match host.list_tools_for(name).await {
            Ok(tools) => Reply::ok(json!({
                "extension": name,
                "tools": tools.into_iter().map(|t| json!({
                    "id": t.name,
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })).collect::<Vec<_>>(),
            })),
            Err(e) => Reply::err(ipc_err(e)),
        },
        None => {
            let aggregated = host.aggregate_tools().await;
            Reply::ok(json!({ "tools": aggregated }))
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// 6. ext.tools.call
// ────────────────────────────────────────────────────────────────────

pub async fn handle_tools_call(host: Arc<Host>, payload: Value) -> Reply {
    let Some(extension) = payload.get("extension").and_then(Value::as_str) else {
        return Reply::err(IpcError::new(
            "bad_request",
            "`extension` (string) required",
        ));
    };
    let Some(tool) = payload.get("tool").and_then(Value::as_str) else {
        return Reply::err(IpcError::new("bad_request", "`tool` (string) required"));
    };
    let arguments = payload.get("arguments").cloned().unwrap_or(json!({}));
    match host.call_tool(extension, tool, arguments).await {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(ipc_err(e)),
    }
}

// ────────────────────────────────────────────────────────────────────
// 7. ext.health
// ────────────────────────────────────────────────────────────────────

pub async fn handle_health(host: Arc<Host>, payload: Value) -> Reply {
    let Some(extension) = payload.get("extension").and_then(Value::as_str) else {
        return Reply::err(IpcError::new(
            "bad_request",
            "`extension` (string) required",
        ));
    };
    match host.ping(extension).await {
        Ok(()) => Reply::ok(json!({ "extension": extension, "ok": true })),
        Err(e) => Reply::err(ipc_err(e)),
    }
}

// ────────────────────────────────────────────────────────────────────
// 8. ext.restart
// ────────────────────────────────────────────────────────────────────

pub async fn handle_restart(host: Arc<Host>, payload: Value) -> Reply {
    let Some(extension) = payload.get("extension").and_then(Value::as_str) else {
        return Reply::err(IpcError::new(
            "bad_request",
            "`extension` (string) required",
        ));
    };
    match host.restart(extension).await {
        Ok(s) => Reply::ok(serde_json::to_value(s).unwrap_or(json!({}))),
        Err(e) => Reply::err(ipc_err(e)),
    }
}

// ────────────────────────────────────────────────────────────────────
// 9. extensions.list_panels — union of every extension's UI panels
// ────────────────────────────────────────────────────────────────────

pub async fn handle_list_panels(host: Arc<Host>, _payload: Value) -> Reply {
    let panels = host.list_panels().await;
    Reply::ok(json!({ "panels": panels }))
}

// ────────────────────────────────────────────────────────────────────
// 10. ext.resources.list — declared resources for the harness verb overlay
// ────────────────────────────────────────────────────────────────────

/// Read-only: return every extension's parsed `resources[]` declarations
/// (Slice 5a). Optional `{extension}` filter. Works for disabled
/// extensions (static parse, no spawn). The harness calls this on
/// extension lifecycle events to (un)register verb-layer resources.
pub async fn handle_resources_list(host: Arc<Host>, payload: Value) -> Reply {
    let only = payload.get("extension").and_then(Value::as_str);
    let resources = host.list_resource_declarations(only).await;
    Reply::ok(json!({ "resources": resources }))
}

// ────────────────────────────────────────────────────────────────────
// 10. ext.events — streaming
// ────────────────────────────────────────────────────────────────────

pub async fn handle_events(host: Arc<Host>, _payload: Value, sender: StreamSender) {
    let mut rx = host.event_subscriber();
    let idle_tick = Duration::from_secs(20);
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(ev) => {
                    let frame = serde_json::to_value(&ev).unwrap_or(json!({"kind":"unknown"}));
                    if sender.send(Ok(frame)).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let frame = json!({
                        "kind": "lagged",
                        "skipped": n,
                        "at": chrono::Utc::now().to_rfc3339(),
                    });
                    // Send failure means client dropped — next iteration
                    // will catch via sender.is_closed(); not a real error.
                    let _ = sender.send(Ok(frame)).await; // wylde-check: discard-result-ok
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return;
                }
            },
            _ = tokio::time::sleep(idle_tick) => {
                // Stay quiet; server-side framing already heartbeats.
                // Use this tick to detect a closed client channel
                // earlier than the next event would.
                if sender.is_closed() {
                    return;
                }
            }
            _ = sender.closed() => return,
        }
    }
}

// Convenience: build a LifecycleEvent JSON without leaking the type.
#[allow(dead_code)]
fn event_to_value(ev: &LifecycleEvent) -> Value {
    serde_json::to_value(ev).unwrap_or(Value::Null)
}
