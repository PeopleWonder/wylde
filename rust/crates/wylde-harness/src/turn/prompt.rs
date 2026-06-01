//! Chat-turn system-prompt assembly — Rust port of the tool-catalog
//! half of `Core/harness/turn/_request_build.py::_build_system_prompt`.
//!
//! ## Why this exists
//!
//! The Python driver injected a system prompt that (a) told the model
//! it is Wylde and may call tools, and (b) listed the available tools
//! with their descriptions and arg schemas. The Phase-5 Rust driver
//! dropped this — it sent the model a bare `[{"role":"user"}]` message
//! with no mention of tools, so the model never emitted tool-call JSON
//! and the salvage parser had nothing to recover. This module restores
//! the system prompt (the "prompt half" of Phase 6).
//!
//! ## Scope
//!
//! Only the base-instruction + tool-catalog block is ported. The
//! memory-slot stack (`_build_system_prompt_with_slots` — long-term,
//! workspace, short-term, persona, RAG) is NOT ported here; those
//! slots read from stores still deferred to Phase 7.
//!
//! The JSON shape advertised to the model — `{"name": ...,
//! "arguments": {...}}` — is exactly what
//! [`crate::turn::salvage::parse_one_call`] recovers from assistant
//! content, so a model that follows the instruction lands cleanly in
//! the salvage path.
//!
//! ## Native `tools:` field (phase-6-native-tools)
//!
//! Smaller models emit in-content tool-call JSON the salvage parser
//! recovers; capable models (qwen2.5:7b, the llama3.2 family, …) ignore
//! the prompt instruction and only honour Ollama's native `tools:`
//! request field, replying on `message.tool_calls`. Both paths now
//! coexist: [`build_system_prompt`] drives the salvage path and
//! [`build_tools_field`] builds the OpenAI-style function specs Ollama
//! accepts on the request body. The two are built from the same catalog
//! payload and apply the same deferred-tool filter.

use serde_json::{json, Value};

/// Cap on the number of tools listed in the catalog block. Mirrors
/// Python's `tools_catalog[:60]` — bounded to keep the prompt small.
const MAX_CATALOG_TOOLS: usize = 60;

/// Build the chat-turn system prompt from a `tools.list` catalog
/// payload (the output of [`crate::tooling::runner::catalog_payload`]).
///
/// Lists each *active* tool by its dotted `name` — the form the model
/// emits and the salvage parser resolves — followed by a compact arg
/// schema and the tool description. Deferred tools are skipped: they
/// return a `phase_<n>_deferred` error on dispatch, so advertising
/// them would only invite calls that can't succeed.
pub fn build_system_prompt(catalog: &[Value]) -> String {
    let mut tool_lines: Vec<String> = Vec::new();

    for tool in catalog.iter().take(MAX_CATALOG_TOOLS) {
        if tool.get("status").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| tool.get("id").and_then(Value::as_str))
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let desc = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let args = render_arg_schema(tool.get("parameters"));
        if args.is_empty() {
            tool_lines.push(format!("- {name}(): {desc}"));
        } else {
            tool_lines.push(format!("- {name}({args}): {desc}"));
        }
    }

    let tool_block = if tool_lines.is_empty() {
        "(no tools available)".to_owned()
    } else {
        tool_lines.join("\n")
    };

    format!(
        "You are Wylde, a locally-hosted assistant. You can call tools \
         to take actions or retrieve information. When you need a tool, \
         respond with a single JSON object and nothing else, of the \
         form {{\"name\": \"<tool_name>\", \"arguments\": {{ ... }}}} — \
         use the exact tool name from the list below. Otherwise produce \
         a direct answer in plain text.\n\n\
         Memory rule: the system automatically tracks important context \
         from your conversation through a post-turn extraction pass — \
         you do not need to call memory.* tools to record things you \
         judge interesting. Use memory.long_term.save / \
         memory.workspace.save / memory.update / memory.delete ONLY \
         when the user has explicitly asked you to modify memory (e.g., \
         \"save this to memory\", \"remember that...\", \"forget X\", \
         \"update what you remember about Y\"). memory.search is fine \
         to call any time you need to look something up.\n\n\
         Available tools:\n{tool_block}"
    )
}

/// Render a tool's `parameters` array (`[{name, type, required,
/// description, default}, ...]`) into a compact, single-line signature
/// fragment, e.g. `path: string, content: string, recursive?: bool`.
///
/// Optional parameters are suffixed with `?`. Returns an empty string
/// when there are no parameters (or the value isn't an array).
fn render_arg_schema(parameters: Option<&Value>) -> String {
    let Some(Value::Array(params)) = parameters else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for p in params {
        let pname = p.get("name").and_then(Value::as_str).unwrap_or("");
        if pname.is_empty() {
            continue;
        }
        let ptype = p.get("type").and_then(Value::as_str).unwrap_or("any");
        let required = p
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let opt_marker = if required { "" } else { "?" };
        parts.push(format!("{pname}{opt_marker}: {ptype}"));
    }
    parts.join(", ")
}

/// Build the native Ollama `tools:` request field from a `tools.list`
/// catalog payload — the OpenAI-style function-calling shape Ollama
/// accepts:
///
/// ```json
/// [{"type": "function",
///   "function": {"name": "time.now", "description": "...",
///                "parameters": {"type": "object", "properties": {...},
///                               "required": [...]}}}]
/// ```
///
/// Capable models reply on `message.tool_calls` when this field is
/// present; the salvage path stays the fallback for models that emit
/// the call as content instead. Deferred tools are skipped (same filter
/// as [`build_system_prompt`]) and the same `MAX_CATALOG_TOOLS` cap
/// applies, so the two advertised tool sets stay in lockstep.
pub fn build_tools_field(catalog: &[Value]) -> Vec<Value> {
    let mut tools: Vec<Value> = Vec::new();

    for tool in catalog.iter().take(MAX_CATALOG_TOOLS) {
        if tool.get("status").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| tool.get("id").and_then(Value::as_str))
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let desc = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        tools.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": json_schema_for(tool.get("parameters")),
            },
        }));
    }

    tools
}

/// Translate a tool's `parameters` array (`[{name, type, required,
/// description, default}, ...]`) into a JSON-schema `object` node:
/// `{type: "object", properties: {...}, required: [...]}`.
///
/// Param `type` strings are normalised to JSON-schema primitive names
/// via [`json_schema_type`]. An absent/non-array `parameters` yields an
/// empty-object schema so the model knows the tool takes no arguments.
fn json_schema_for(parameters: Option<&Value>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    if let Some(Value::Array(params)) = parameters {
        for p in params {
            let pname = p.get("name").and_then(Value::as_str).unwrap_or("");
            if pname.is_empty() {
                continue;
            }
            let ptype = p.get("type").and_then(Value::as_str).unwrap_or("string");
            let desc = p.get("description").and_then(Value::as_str).unwrap_or("");
            properties.insert(
                pname.to_owned(),
                json!({"type": json_schema_type(ptype), "description": desc}),
            );
            if p.get("required").and_then(Value::as_bool).unwrap_or(false) {
                required.push(Value::String(pname.to_owned()));
            }
        }
    }

    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
    })
}

/// Normalise a catalog param `type` string to a JSON-schema primitive
/// type. The Rust tool catalog already uses JSON-schema names
/// (`string`, `number`, `array`, `boolean`); the extra aliases keep the
/// translation robust if a manifest-sourced type leaks a Python-ish
/// spelling (`int`, `bool`, `list`, `dict`). Unknown types fall back to
/// `string` — the safe default for an LLM arg.
fn json_schema_type(t: &str) -> &str {
    match t {
        "string" | "str" => "string",
        "number" | "float" | "double" => "number",
        "integer" | "int" => "integer",
        "boolean" | "bool" => "boolean",
        "array" | "list" => "array",
        "object" | "dict" | "map" => "object",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_catalog() -> Vec<Value> {
        vec![
            json!({
                "id": "read_file",
                "name": "fs.read_file",
                "group": "fs",
                "description": "Read the text contents of a file.",
                "parameters": [
                    {"name": "path", "type": "string", "required": true,
                     "description": "Path to the file"}
                ],
                "destructive": false,
                "status": "active",
                "deferred_phase": null,
            }),
            json!({
                "id": "visual_caption",
                "name": "visual.caption",
                "group": "visual",
                "description": "Caption an image.",
                "parameters": [],
                "destructive": false,
                "status": "deferred",
                "deferred_phase": "11",
            }),
        ]
    }

    #[test]
    fn prompt_lists_active_tool_by_dotted_name() {
        let prompt = build_system_prompt(&sample_catalog());
        assert!(prompt.contains("Available tools:"));
        assert!(prompt.contains("fs.read_file"));
        assert!(prompt.contains("path: string"));
    }

    #[test]
    fn prompt_skips_deferred_tools() {
        let prompt = build_system_prompt(&sample_catalog());
        // `visual.caption` is deferred and appears nowhere in the base
        // instruction text, so its total absence proves the catalog
        // block excluded it.
        assert!(
            !prompt.contains("visual.caption"),
            "deferred tools must not be advertised: {prompt}"
        );
    }

    #[test]
    fn prompt_advertises_salvage_json_shape() {
        let prompt = build_system_prompt(&sample_catalog());
        // The shape must match what salvage::parse_one_call recovers.
        assert!(prompt.contains("\"name\""));
        assert!(prompt.contains("\"arguments\""));
    }

    #[test]
    fn prompt_handles_empty_catalog() {
        let prompt = build_system_prompt(&[]);
        assert!(prompt.contains("(no tools available)"));
    }

    #[test]
    fn render_arg_schema_marks_optional_params() {
        let params = json!([
            {"name": "path", "type": "string", "required": true},
            {"name": "depth", "type": "int", "required": false},
        ]);
        let rendered = render_arg_schema(Some(&params));
        assert_eq!(rendered, "path: string, depth?: int");
    }

    #[test]
    fn render_arg_schema_empty_for_no_params() {
        assert_eq!(render_arg_schema(Some(&json!([]))), "");
        assert_eq!(render_arg_schema(None), "");
    }

    // ── Fix B: native Ollama `tools:` field ──────────────────────────────

    #[test]
    fn tools_field_emits_openai_function_shape() {
        let tools = build_tools_field(&sample_catalog());
        // Only the active tool surfaces; the deferred one is filtered.
        assert_eq!(tools.len(), 1, "expected one active tool: {tools:?}");
        let t = &tools[0];
        assert_eq!(t["type"], "function");
        assert_eq!(t["function"]["name"], "fs.read_file");
        assert_eq!(t["function"]["description"], "Read the text contents of a file.");
    }

    #[test]
    fn tools_field_translates_params_to_json_schema() {
        let tools = build_tools_field(&sample_catalog());
        let params = &tools[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        // `path` is a required string parameter.
        assert_eq!(params["properties"]["path"]["type"], "string");
        let required = params["required"].as_array().expect("required array");
        assert!(required.contains(&json!("path")));
    }

    #[test]
    fn tools_field_skips_deferred_tools() {
        let tools = build_tools_field(&sample_catalog());
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(
            !names.contains(&"visual.caption"),
            "deferred tools must not be advertised: {names:?}"
        );
    }

    #[test]
    fn tools_field_no_params_yields_empty_object_schema() {
        let catalog = vec![json!({
            "id": "now", "name": "time.now", "group": "time",
            "description": "Current time.", "parameters": [],
            "destructive": false, "status": "active", "deferred_phase": null,
        })];
        let tools = build_tools_field(&catalog);
        let params = &tools[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"], json!({}));
        assert_eq!(params["required"], json!([]));
    }

    #[test]
    fn json_schema_type_normalises_aliases() {
        assert_eq!(json_schema_type("int"), "integer");
        assert_eq!(json_schema_type("bool"), "boolean");
        assert_eq!(json_schema_type("list"), "array");
        assert_eq!(json_schema_type("number"), "number");
        // Unknown → safe string default.
        assert_eq!(json_schema_type("widget"), "string");
    }
}
