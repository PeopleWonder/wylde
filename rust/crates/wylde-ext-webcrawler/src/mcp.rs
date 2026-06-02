//! MCP-over-stdio server loop.
//!
//! Speaks the same minimal MCP subset the Python `_shim/server.py` does, so
//! the `wylde-extension-bridge` host can drive this binary identically:
//! `initialize`, `notifications/initialized`, `tools/list`, `tools/call`,
//! `ping`. Stdout carries newline-delimited JSON-RPC 2.0; stderr is reserved
//! for logs (the bridge captures it).
//!
//! The protocol constants and envelope shapes are pinned to match the shim
//! exactly (plan risk #3): `protocolVersion: "2025-11-25"`,
//! `capabilities.tools.listChanged: false`, and the `tools/call` result
//! `{content:[{type:"text",text:<json>}], structuredContent, isError}`.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::tools;

/// MCP spec version the shim advertises. Kept identical so the host's
/// per-extension N/N-1 compat policy accepts us unchanged.
pub const MCP_SPEC_VERSION: &str = "2025-11-25";
pub const SERVER_NAME: &str = "wylde-ext-webcrawler";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the stdio server until stdin closes (EOF) — the bridge's signal to
/// shut the child down.
pub async fn serve() -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let preview: String = line.chars().take(120).collect();
                tracing::warn!("ignoring non-JSON line: {preview} — {e}");
                continue;
            }
        };

        let method = match msg.get("method").and_then(Value::as_str) {
            Some(m) => m,
            // A response coming back to us, or a malformed frame — ignore.
            None => continue,
        };
        let id = msg.get("id").cloned();
        let params = match msg.get("params") {
            Some(Value::Null) | None => json!({}),
            Some(p) => p.clone(),
        };

        // No id ⇒ notification. Ack by silence (`notifications/initialized`).
        let id = match id {
            None | Some(Value::Null) => continue,
            Some(id) => id,
        };

        let response = dispatch(method, id, params).await;
        let mut frame = serde_json::to_string(&response)?;
        frame.push('\n');
        stdout.write_all(frame.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn dispatch(method: &str, id: Value, params: Value) -> Value {
    match method {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": MCP_SPEC_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            }),
        ),
        "tools/list" => ok(id, json!({ "tools": tool_catalog() })),
        "tools/call" => handle_tools_call(id, params).await,
        "ping" => ok(id, json!({})),
        other => err(id, -32601, &format!("method `{other}` not implemented"), None),
    }
}

async fn handle_tools_call(id: Value, params: Value) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_owned(),
        None => return err(id, -32602, "missing string `name`", None),
    };
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(Value::Object(_)) => params.get("arguments").cloned().unwrap(),
        Some(_) => return err(id, -32602, "`arguments` must be an object", None),
    };

    let result = match name.as_str() {
        "fetch" => tools::run_fetch(arguments).await,
        "scrape" => tools::run_scrape(arguments).await,
        "extract" => tools::run_extract(arguments).await,
        other => return err(id, -32601, &format!("unknown tool `{other}`"), None),
    };

    // MCP `tools/call` envelope — identical to the shim's wrapping.
    let structured = if result.is_object() {
        result.clone()
    } else {
        json!({ "value": result })
    };
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap_or_default() }],
            "structuredContent": structured,
            "isError": false,
        }),
    )
}

/// The three-tool catalog with the exact `inputSchema`s the shim emits from
/// `Extensions/Webcrawler/manifest.json` (`parameters[]` → JSON Schema).
fn tool_catalog() -> Value {
    json!([
        {
            "name": "fetch",
            "description": "Fetch raw contents from a URL. Returns the body as text or parsed JSON depending on the format parameter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "format": { "type": "string", "description": "Response format: 'text' or 'json'", "default": "text" },
                    "timeout": { "type": "number", "description": "Request timeout in seconds", "default": 10 }
                },
                "required": ["url"]
            }
        },
        {
            "name": "scrape",
            "description": "Scrape HTML content from a URL with optional CSS selectors. Returns the raw page plus selector-extracted text values.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to scrape" },
                    "selectors": { "type": "array", "description": "CSS selectors to extract (optional)" },
                    "timeout": { "type": "number", "description": "Request timeout in seconds", "default": 10 }
                },
                "required": ["url"]
            }
        },
        {
            "name": "extract",
            "description": "Extract structured data from HTML using a rule set ({field: {selector, attribute, multiple}}). Accepts either a 'url' to fetch first or raw 'html'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch and extract from (or use html parameter)" },
                    "html": { "type": "string", "description": "Raw HTML content (alternative to url)" },
                    "extraction_rules": { "type": "object", "description": "Rules for extracting structured data" }
                },
                "required": ["extraction_rules"]
            }
        }
    ])
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": e })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_advertises_shim_protocol() {
        let r = dispatch("initialize", json!(1), json!({})).await;
        assert_eq!(r["result"]["protocolVersion"], MCP_SPEC_VERSION);
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(r["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn tools_list_has_three_tools() {
        let r = dispatch("tools/list", json!(2), json!({})).await;
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["fetch", "scrape", "extract"]);
        // Every tool carries an object inputSchema.
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let r = dispatch("ping", json!(3), json!({})).await;
        assert_eq!(r["result"], json!({}));
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let r = dispatch("frobnicate", json!(4), json!({})).await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_call_extract_round_trips_envelope() {
        let r = dispatch(
            "tools/call",
            json!(5),
            json!({
                "name": "extract",
                "arguments": {
                    "html": "<h1>Hi</h1>",
                    "extraction_rules": { "h": { "selector": "h1" } }
                }
            }),
        )
        .await;
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["structuredContent"]["status"], "ok");
        assert_eq!(r["result"]["structuredContent"]["extracted_data"]["h"], "Hi");
        // `content[0].text` is the JSON-stringified structured result.
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("\"status\""));
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_is_method_not_found() {
        let r = dispatch("tools/call", json!(6), json!({ "name": "nope" })).await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_call_non_object_arguments_rejected() {
        let r = dispatch(
            "tools/call",
            json!(7),
            json!({ "name": "fetch", "arguments": "oops" }),
        )
        .await;
        assert_eq!(r["error"]["code"], -32602);
    }
}
