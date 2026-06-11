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

/// Canonical ids of the named tools that remain advertised after the
/// Slice-6 verb cutover, *alongside* the eight `wylde_*` verb tools.
/// Everything else with a resource equivalent (memory, rag, graph, fs,
/// code search) is retired from the model-facing catalog — its handler
/// stays registered and dispatchable, just no longer advertised, and is
/// reached through the verbs (`docs/wylde-phase5-cutover.md`).
///
/// After Slice 4b only **one** principled category survives (R6 in the
/// consolidation plan):
///
/// 1. **Imperative — permanent.** Stateful device-lifecycle triggers with
///    no resource identity (open/close an OS audio device, start/stop a
///    listener thread). These are named *by design* and never collapse
///    into a verb.
///
/// The former "awaiting resource migration — temporary" category is now
/// **empty**: Slice 4b registered the `model` / `time` / `diff` / `voice`
/// resources, so the 11 ollama/time/diff/voice-inference tools that used
/// to sit here are retired from advertising and reached through the verbs
/// (`docs/wylde-phase6-cutover.md`). Their handlers stay registered and
/// dispatchable — the retirement is advertising-only.
const SURVIVING_NAMED_TOOLS: &[&str] = &[
    // ── imperative (permanent) — voice device lifecycle ──
    "voice_mic_start",
    "voice_mic_stop",
    "voice_wakeword_start",
    "voice_wakeword_stop",
];

/// Whether a catalog row should be advertised to the model.
///
/// Legacy mode (`verb_mode == false`) advertises every *active* tool, as
/// before the cutover. Verb mode advertises only the verb tools (group
/// `"verbs"`) plus the [`SURVIVING_NAMED_TOOLS`] tail. The caller has
/// already filtered to `status == "active"`.
fn advertise(tool: &Value, verb_mode: bool) -> bool {
    if !verb_mode {
        return true;
    }
    if tool.get("group").and_then(Value::as_str) == Some("verbs") {
        return true;
    }
    let id = tool.get("id").and_then(Value::as_str).unwrap_or("");
    SURVIVING_NAMED_TOOLS.contains(&id)
}

/// The verb-mode guidance block prepended to the tool catalog: how to
/// discover resource types, and the one-sentence rule that separates the
/// resource verbs from the surviving named tools (plan R6).
const VERB_MODE_GUIDANCE: &str = "\
Tool model: operate on resources with eight generic verbs — \
wylde_describe, wylde_list, wylde_get, wylde_create, wylde_update, \
wylde_delete, wylde_search, wylde_execute — each taking a `resource_type`. \
The legal `resource_type` values are NOT in this prompt: call \
wylde_describe first (no argument) to list them, then wylde_describe with \
one `resource_type` for its fields and per-verb notes. The handful of \
named tools below are the exceptions to the verb model — they either \
start/stop a live device or run an action with no resource identity. \
Everything else is a resource verb.";

/// Build the chat-turn system prompt from a `tools.list` catalog
/// payload (the output of [`crate::tooling::runner::catalog_payload`]).
///
/// Lists each *active* tool by its dotted `name` — the form the model
/// emits and the salvage parser resolves — followed by a compact arg
/// schema and the tool description. Deferred tools are skipped: they
/// return a `phase_<n>_deferred` error on dispatch, so advertising
/// them would only invite calls that can't succeed.
///
/// `verb_mode` is the Slice-6 cutover gate
/// ([`crate::tooling::resource::verb_mode_active`]). When on, only the
/// verb tools and the [`SURVIVING_NAMED_TOOLS`] tail are advertised
/// (resource-backed named tools are retired) and the verb-discovery
/// guidance is prepended; when off, every active tool is listed (legacy).
pub fn build_system_prompt(catalog: &[Value], verb_mode: bool) -> String {
    let mut tool_lines: Vec<String> = Vec::new();

    // Filter first, cap second: the `MAX_CATALOG_TOOLS` bound applies to the
    // *advertised* set, not the raw (alphabetically-sorted) catalog — the
    // `wylde_*` verbs sort last, so a pre-filter cap would chop them off.
    for tool in catalog
        .iter()
        .filter(|t| t.get("status").and_then(Value::as_str) == Some("active"))
        .filter(|t| advertise(t, verb_mode))
        .take(MAX_CATALOG_TOOLS)
    {
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

    // The verb-cutover prompt swaps the named-tool guidance block and the
    // memory rule for their verb-shaped equivalents; legacy mode keeps the
    // original wording so a model primed on the old prompt is unaffected.
    let guidance_block = if verb_mode {
        format!("{VERB_MODE_GUIDANCE}\n\n")
    } else {
        String::new()
    };

    // B9: the base instruction and the memory rule resolve through the
    // prompts catalog/override store, so the Settings prompt editor can
    // tune them without a rebuild. Catalog defaults are byte-identical to
    // the pre-B9 hardcoded strings (pinned by the B11 goldens). The
    // verb-mode guidance stays a const — its wording is mechanically
    // coupled to the verb registry, not a style knob.
    let base = crate::prompts::store::effective_prompt("chat.system_base");
    let memory_rule = crate::prompts::store::effective_prompt(if verb_mode {
        "chat.memory_rule"
    } else {
        "chat.memory_rule_legacy"
    });

    format!(
        "{base}\n\n\
         {guidance_block}{memory_rule}\n\n\
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
        let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
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
pub fn build_tools_field(catalog: &[Value], verb_mode: bool) -> Vec<Value> {
    let mut tools: Vec<Value> = Vec::new();

    // Filter first, cap second — see [`build_system_prompt`]: the cap must
    // bound the advertised set so the `wylde_*` verbs (last alphabetically)
    // are never chopped off by it.
    for tool in catalog
        .iter()
        .filter(|t| t.get("status").and_then(Value::as_str) == Some("active"))
        .filter(|t| advertise(t, verb_mode))
        .take(MAX_CATALOG_TOOLS)
    {
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
        let prompt = build_system_prompt(&sample_catalog(), false);
        assert!(prompt.contains("Available tools:"));
        assert!(prompt.contains("fs.read_file"));
        assert!(prompt.contains("path: string"));
    }

    #[test]
    fn prompt_skips_deferred_tools() {
        let prompt = build_system_prompt(&sample_catalog(), false);
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
        let prompt = build_system_prompt(&sample_catalog(), false);
        // The shape must match what salvage::parse_one_call recovers.
        assert!(prompt.contains("\"name\""));
        assert!(prompt.contains("\"arguments\""));
    }

    #[test]
    fn prompt_handles_empty_catalog() {
        let prompt = build_system_prompt(&[], false);
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
        let tools = build_tools_field(&sample_catalog(), false);
        // Only the active tool surfaces; the deferred one is filtered.
        assert_eq!(tools.len(), 1, "expected one active tool: {tools:?}");
        let t = &tools[0];
        assert_eq!(t["type"], "function");
        assert_eq!(t["function"]["name"], "fs.read_file");
        assert_eq!(
            t["function"]["description"],
            "Read the text contents of a file."
        );
    }

    #[test]
    fn tools_field_translates_params_to_json_schema() {
        let tools = build_tools_field(&sample_catalog(), false);
        let params = &tools[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        // `path` is a required string parameter.
        assert_eq!(params["properties"]["path"]["type"], "string");
        let required = params["required"].as_array().expect("required array");
        assert!(required.contains(&json!("path")));
    }

    #[test]
    fn tools_field_skips_deferred_tools() {
        let tools = build_tools_field(&sample_catalog(), false);
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
        let tools = build_tools_field(&catalog, false);
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

    // ── Slice 6: verb-cutover advertising filter ─────────────────────────

    fn row(id: &str, name: &str, group: &str) -> Value {
        json!({
            "id": id, "name": name, "group": group,
            "description": format!("{name} description"),
            "parameters": [], "destructive": false,
            "status": "active", "deferred_phase": null,
        })
    }

    /// A mixed catalog: one verb, one surviving named tool (imperative
    /// voice device trigger — the only survivor category after Slice 4b),
    /// two retired resource-backed tools (`memory.search` and `time.now`,
    /// the latter retired when the `time` resource landed in 4b), and a
    /// deferred tool.
    fn mixed_catalog() -> Vec<Value> {
        vec![
            row("wylde_search", "wylde_search", "verbs"),
            row("voice_mic_start", "voice.mic.start", "voice"),
            row("time_now", "time.now", "time"),
            row("memory_search", "memory.search", "memory"),
            json!({
                "id": "screenshot", "name": "visual.screenshot", "group": "visual",
                "description": "shot", "parameters": [], "destructive": true,
                "status": "deferred", "deferred_phase": "11",
            }),
        ]
    }

    #[test]
    fn verb_mode_advertises_verbs_and_survivors_only() {
        let prompt = build_system_prompt(&mixed_catalog(), true);
        // verb tool + the imperative survivor are advertised
        assert!(prompt.contains("wylde_search"), "verb missing: {prompt}");
        assert!(
            prompt.contains("voice.mic.start"),
            "imperative survivor missing"
        );
        // resource-backed named tools are retired from advertising —
        // including time.now, now backed by the `time` resource (4b).
        assert!(
            !prompt.contains("memory.search"),
            "resource-backed tool must be retired in verb mode: {prompt}"
        );
        assert!(
            !prompt.contains("time.now"),
            "time.now must be retired after the 4b `time` resource: {prompt}"
        );
        // deferred stays excluded as always
        assert!(!prompt.contains("visual.screenshot"));
        // verb-mode guidance is present
        assert!(
            prompt.contains("wylde_describe first"),
            "describe hint missing"
        );
    }

    #[test]
    fn legacy_mode_still_advertises_resource_backed_tools() {
        let prompt = build_system_prompt(&mixed_catalog(), false);
        assert!(
            prompt.contains("memory.search"),
            "legacy mode must keep named tools"
        );
        assert!(prompt.contains("wylde_search"));
        // No verb-mode guidance in legacy mode.
        assert!(!prompt.contains("wylde_describe first"));
    }

    #[test]
    fn verb_mode_memory_rule_references_verbs_not_named_tools() {
        let prompt = build_system_prompt(&mixed_catalog(), true);
        assert!(prompt.contains("wylde_create(\"memory\""));
        assert!(!prompt.contains("memory.long_term.save"));
    }

    #[test]
    fn tools_field_verb_mode_filters_to_verbs_and_survivors() {
        let tools = build_tools_field(&mixed_catalog(), true);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"wylde_search"));
        assert!(names.contains(&"voice.mic.start"));
        assert!(
            !names.contains(&"memory.search"),
            "retired tool leaked: {names:?}"
        );
        assert!(
            !names.contains(&"time.now"),
            "time.now retired in 4b: {names:?}"
        );
        assert_eq!(
            names.len(),
            2,
            "exactly verb + 1 imperative survivor: {names:?}"
        );
    }
}
