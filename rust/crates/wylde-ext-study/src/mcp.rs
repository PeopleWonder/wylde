//! MCP-over-stdio server loop.
//!
//! Speaks the same minimal MCP subset the Python `_shim/server.py` does, so
//! the `wylde-extension-bridge` host can drive this binary identically:
//! `initialize`, `notifications/initialized`, `tools/list`, `tools/call`,
//! `ping`. Stdout carries newline-delimited JSON-RPC 2.0; stderr is reserved
//! for logs (the bridge captures it).
//!
//! The protocol constants and envelope shapes are pinned to match the shim
//! exactly: `protocolVersion: "2025-11-25"`,
//! `capabilities.tools.listChanged: false`, and the `tools/call` result
//! `{content:[{type:"text",text:<json>}], structuredContent, isError}`.
//!
//! The five `inputSchema`s are the `parameters[]` blocks of
//! `Extensions/Wylde_Study/manifest.json` lifted into JSON Schema — same
//! transform the Python shim applies — so the host's tool catalog is
//! unchanged across the flip.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::harness::{HarnessClient, PipeClient};
use crate::tools;

/// MCP spec version the shim advertises. Kept identical so the host's
/// per-extension N/N-1 compat policy accepts us unchanged.
pub const MCP_SPEC_VERSION: &str = "2025-11-25";
pub const SERVER_NAME: &str = "wylde-ext-study";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the stdio server until stdin closes (EOF) — the bridge's signal to
/// shut the child down.
pub async fn serve() -> anyhow::Result<()> {
    let client = PipeClient::from_config();
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

        let response = dispatch(&client, method, id, params).await;
        let mut frame = serde_json::to_string(&response)?;
        frame.push('\n');
        stdout.write_all(frame.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn dispatch<C: HarnessClient>(client: &C, method: &str, id: Value, params: Value) -> Value {
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
        "tools/call" => handle_tools_call(client, id, params).await,
        "ping" => ok(id, json!({})),
        other => err(id, -32601, &format!("method `{other}` not implemented"), None),
    }
}

async fn handle_tools_call<C: HarnessClient>(client: &C, id: Value, params: Value) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_owned(),
        None => return err(id, -32602, "missing string `name`", None),
    };
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(Value::Object(_)) => params.get("arguments").cloned().unwrap(),
        Some(_) => return err(id, -32602, "`arguments` must be an object", None),
    };

    let result = match tools::dispatch_tool(client, &name, arguments).await {
        Some(r) => r,
        None => return err(id, -32601, &format!("unknown tool `{name}`"), None),
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

/// The five-tool catalog with the exact `inputSchema`s derived from
/// `Extensions/Wylde_Study/manifest.json` (`parameters[]` → JSON Schema).
fn tool_catalog() -> Value {
    json!([
        {
            "name": "study_index_page",
            "description": "Index a web page (title + URL + extracted text) into the Wylde memory graph as an episodic memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Page URL" },
                    "title": { "type": "string", "description": "Page title" },
                    "text": { "type": "string", "description": "Extracted text content" },
                    "session_id": { "type": "string", "description": "Session id grouping pages indexed together" }
                },
                "required": ["url", "text"]
            }
        },
        {
            "name": "study_query",
            "description": "Answer a question against the indexed corpus. Wraps rag.search and returns the top-k matching chunks for the LLM (or extension UI) to ground on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": { "type": "string", "description": "Question" },
                    "limit": { "type": "number", "description": "Max chunks (1..50)", "default": 8 }
                },
                "required": ["q"]
            }
        },
        {
            "name": "study_summarize",
            "description": "Summarize the supplied text via the configured LLM. Returns a short structured summary + key points.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to summarize" },
                    "model": { "type": "string", "description": "Model name (defaults to harness default)" },
                    "max_words": { "type": "number", "description": "Target summary length", "default": 150 }
                },
                "required": ["text"]
            }
        },
        {
            "name": "study_explain",
            "description": "Explain a concept or selection of text in plain language via the LLM.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Concept or excerpt to explain" },
                    "audience": { "type": "string", "description": "Audience hint, e.g. 'high school', 'expert'", "default": "general" },
                    "model": { "type": "string", "description": "Model name (defaults to harness default)" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "study_flashcards",
            "description": "Generate Q/A flashcards from the supplied text via the LLM. Returns a list of {front, back} objects.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Source material" },
                    "count": { "type": "number", "description": "How many cards to generate (1..50)", "default": 8 },
                    "model": { "type": "string", "description": "Model name (defaults to harness default)" }
                },
                "required": ["text"]
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
    use wylde_shared::ipc::IpcError;

    /// Minimal mock so the protocol-level tests can drive `tools/call`
    /// without a live harness.
    struct MockClient {
        reply: Value,
    }
    impl HarnessClient for MockClient {
        async fn call(&self, _action: &str, _payload: Value) -> Result<Value, IpcError> {
            Ok(self.reply.clone())
        }
    }
    fn mock() -> MockClient {
        MockClient { reply: json!({ "status": "ok", "memory_id": "m" }) }
    }

    #[tokio::test]
    async fn initialize_advertises_shim_protocol() {
        let r = dispatch(&mock(), "initialize", json!(1), json!({})).await;
        assert_eq!(r["result"]["protocolVersion"], MCP_SPEC_VERSION);
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(r["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn tools_list_has_the_five_study_tools() {
        let r = dispatch(&mock(), "tools/list", json!(2), json!({})).await;
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            [
                "study_index_page",
                "study_query",
                "study_summarize",
                "study_explain",
                "study_flashcards"
            ]
        );
        // Every tool carries an object inputSchema with `text` or `url`/`q`.
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["inputSchema"]["required"].is_array());
        }
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let r = dispatch(&mock(), "ping", json!(3), json!({})).await;
        assert_eq!(r["result"], json!({}));
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let r = dispatch(&mock(), "frobnicate", json!(4), json!({})).await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_call_index_page_round_trips_envelope() {
        let r = dispatch(
            &mock(),
            "tools/call",
            json!(5),
            json!({
                "name": "study_index_page",
                "arguments": { "url": "http://x", "text": "hello" }
            }),
        )
        .await;
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["structuredContent"]["status"], "ok");
        assert_eq!(r["result"]["structuredContent"]["memory_id"], "m");
        // `content[0].text` is the JSON-stringified structured result.
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"status\""));
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_is_method_not_found() {
        let r = dispatch(&mock(), "tools/call", json!(6), json!({ "name": "nope" })).await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_call_non_object_arguments_rejected() {
        let r = dispatch(
            &mock(),
            "tools/call",
            json!(7),
            json!({ "name": "study_query", "arguments": "oops" }),
        )
        .await;
        assert_eq!(r["error"]["code"], -32602);
    }
}
