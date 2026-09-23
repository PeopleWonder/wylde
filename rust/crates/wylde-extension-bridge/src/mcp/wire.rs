//! JSON-RPC 2.0 envelope types + MCP-specific request/response bodies.
//!
//! We use untyped `serde_json::Value` for response `result` blobs where
//! flexibility matters (servers add fields) and named structs only for
//! the shapes we actually consume.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.to_owned(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

impl Notification {
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: method.to_owned(),
            params,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// What the host sends in `initialize.params`.
pub fn build_initialize_params(
    client_name: &str,
    client_version: &str,
    spec_version: &str,
) -> Value {
    json!({
        "protocolVersion": spec_version,
        "capabilities": {
            // Wylde host consumes server-side `tools` and `resources`.
            // It does NOT offer sampling, roots, elicitation, or logging
            // to servers (those are server→host calls that would require
            // routing user-consent flows we don't have a UI for yet).
            "tools": {},
            "resources": {}
        },
        "clientInfo": {
            "name": client_name,
            "version": client_version
        }
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(rename = "serverInfo", default)]
    pub server_info: ServerInfo,
}
