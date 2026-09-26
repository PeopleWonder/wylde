//! OpenAI chat ↔ Ollama `/api/chat` translation (pure; no I/O).
//!
//! Request side: `messages` (text; content-part arrays are flattened to
//! text), `tools`, `tool_choice` (`none` drops the tools; any other value
//! passes the tools through, since Ollama can't force a call), sampling
//! options, `max_tokens`/`max_completion_tokens` → `num_predict`, and
//! `response_format` (`json_object` → `"json"`, `json_schema` → the schema)
//! → Ollama `format`. Response side: assistant text, tool calls (Ollama
//! gives arguments as an object; OpenAI wants a JSON string), finish
//! reasons and usage. Unknown request fields are ignored.

use serde_json::{json, Map, Value};

use super::aliases::Aliases;
use super::errors::OpenAiError;

/// A validated chat request, ready to send to `ollama.chat_stream`.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// The model id the client asked for (echoed in responses).
    pub model: String,
    /// The resolved (alias → real) model id.
    pub target: String,
    /// The Ollama `/api/chat` payload (always `stream: true`).
    pub ollama: Value,
    pub stream: bool,
    pub include_usage: bool,
    /// Tool names offered to the model (empty when there are no tools).
    pub tool_names: Vec<String>,
}

fn bad(msg: impl Into<String>, param: &str) -> OpenAiError {
    OpenAiError::invalid_request(msg, Some(param))
}

/// Flatten OpenAI message content (a string, null, or an array of parts)
/// to text. Only `text` parts are supported.
fn content_text(content: Option<&Value>) -> Result<String, OpenAiError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                match (part.get("type").and_then(Value::as_str), part.get("text")) {
                    (Some("text"), Some(Value::String(t))) => out.push_str(t),
                    _ => return Err(bad("Only text content parts are supported", "messages")),
                }
            }
            Ok(out)
        }
        Some(_) => Err(bad(
            "Message content must be a string or an array of parts",
            "messages",
        )),
    }
}

/// OpenAI assistant `tool_calls` → Ollama `tool_calls` (arguments parsed
/// from the JSON string into an object).
fn tool_calls_to_ollama(calls: &[Value]) -> Vec<Value> {
    calls
        .iter()
        .filter_map(|c| {
            let f = c.get("function")?;
            let name = f.get("name")?.as_str()?;
            let args = match f.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                Some(v @ Value::Object(_)) => v.clone(),
                _ => json!({}),
            };
            Some(json!({"function": {"name": name, "arguments": args}}))
        })
        .collect()
}

/// OpenAI `messages` → Ollama `messages`. A `tool` message gets the
/// `tool_name` Ollama expects, looked up from the assistant call it answers.
fn convert_messages(messages: &[Value]) -> Result<Vec<Value>, OpenAiError> {
    let mut out = Vec::with_capacity(messages.len());
    let mut call_names: Vec<(String, String)> = Vec::new();
    for m in messages {
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("Every message needs a 'role'", "messages"))?;
        let role = if role == "developer" { "system" } else { role };
        let mut msg = json!({"role": role, "content": content_text(m.get("content"))?});
        match role {
            "system" | "user" => {}
            "assistant" => {
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        if let (Some(id), Some(name)) = (
                            c.get("id").and_then(Value::as_str),
                            c.pointer("/function/name").and_then(Value::as_str),
                        ) {
                            call_names.push((id.to_owned(), name.to_owned()));
                        }
                    }
                    msg["tool_calls"] = Value::Array(tool_calls_to_ollama(calls));
                }
            }
            "tool" => {
                let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                if let Some((_, name)) = call_names.iter().find(|(i, _)| i == id) {
                    msg["tool_name"] = json!(name);
                }
            }
            other => {
                return Err(bad(
                    format!("Unsupported message role '{other}'"),
                    "messages",
                ))
            }
        }
        out.push(msg);
    }
    Ok(out)
}

/// Sampling options → Ollama `options`.
fn options(req: &Value) -> Result<Map<String, Value>, OpenAiError> {
    let mut o = Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("seed", "seed"),
        ("presence_penalty", "presence_penalty"),
        ("frequency_penalty", "frequency_penalty"),
    ] {
        if let Some(v) = req.get(from).filter(|v| v.is_number()) {
            o.insert(to.to_owned(), v.clone());
        }
    }
    let max = req
        .get("max_completion_tokens")
        .or_else(|| req.get("max_tokens"))
        .filter(|v| !v.is_null());
    if let Some(v) = max {
        let n = v
            .as_u64()
            .ok_or_else(|| bad("max_tokens must be a positive integer", "max_tokens"))?;
        o.insert("num_predict".to_owned(), json!(n));
    }
    if let Some(stop) = stop_list(req.get("stop"))? {
        o.insert("stop".to_owned(), json!(stop));
    }
    Ok(o)
}

/// `stop` (a string or an array of strings) → a list.
pub fn stop_list(stop: Option<&Value>) -> Result<Option<Vec<String>>, OpenAiError> {
    match stop {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad("stop must be strings", "stop"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(bad("stop must be a string or an array of strings", "stop")),
    }
}

/// `response_format` → Ollama `format`.
fn format(req: &Value) -> Result<Option<Value>, OpenAiError> {
    let Some(rf) = req.get("response_format").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    match rf.get("type").and_then(Value::as_str) {
        Some("text") => Ok(None),
        Some("json_object") => Ok(Some(json!("json"))),
        Some("json_schema") => rf
            .pointer("/json_schema/schema")
            .filter(|s| s.is_object())
            .cloned()
            .map(Some)
            .ok_or_else(|| bad("json_schema.schema must be an object", "response_format")),
        _ => Err(bad("Unsupported response_format type", "response_format")),
    }
}

/// Parse and translate a `/v1/chat/completions` body.
pub fn parse_chat(body: &[u8], aliases: &Aliases) -> Result<ChatRequest, OpenAiError> {
    let req: Value = serde_json::from_slice(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| OpenAiError::invalid_request("Request body must be a JSON object", None))?;
    let model = req
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| bad("'model' is required", "model"))?
        .to_owned();
    if req.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        return Err(bad("Only n=1 is supported", "n"));
    }
    let messages = req
        .get("messages")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| bad("'messages' must be a non-empty array", "messages"))?;
    let target = aliases.resolve(&model).to_owned();

    let mut ollama = json!({
        "model": target,
        "messages": convert_messages(messages)?,
        "stream": true,
        "options": options(&req)?,
        // wylde-ollama knobs: keep the model resident on a client cancel,
        // and never reload it just because this request's options differ.
        "evict_on_cancel": false,
        "pin_load_options": true,
    });
    let tool_choice_none = req.get("tool_choice").and_then(Value::as_str) == Some("none");
    let mut tool_names = Vec::new();
    if let Some(tools) = req
        .get("tools")
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty())
    {
        if !tool_choice_none {
            tool_names = tools
                .iter()
                .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            ollama["tools"] = Value::Array(tools.clone());
        }
    }
    if let Some(f) = format(&req)? {
        ollama["format"] = f;
    }
    Ok(ChatRequest {
        model,
        target,
        ollama,
        stream: req.get("stream").and_then(Value::as_bool).unwrap_or(false),
        include_usage: req
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tool_names,
    })
}

/// Ollama tool calls → OpenAI `tool_calls`, numbered from `first_index`.
/// Ollama may omit call ids; one is minted from the index.
pub fn tool_calls_to_openai(calls: &[Value], first_index: usize) -> Vec<Value> {
    calls
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let index = first_index + i;
            let f = c.get("function").cloned().unwrap_or_else(|| json!({}));
            let args = match f.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned()),
                None => "{}".to_owned(),
            };
            let id = c
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| format!("call_{index}"), str::to_owned);
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": f.get("name").cloned().unwrap_or(json!("")), "arguments": args},
            })
        })
        .collect()
}

/// OpenAI `finish_reason` from Ollama's `done_reason`.
pub fn finish_reason(done_reason: Option<&str>, tool_calls: bool) -> &'static str {
    if tool_calls {
        "tool_calls"
    } else if done_reason == Some("length") {
        "length"
    } else {
        "stop"
    }
}

/// OpenAI `usage` from Ollama's token counts.
pub fn usage(prompt_tokens: u64, completion_tokens: u64) -> Value {
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: Value) -> Result<ChatRequest, OpenAiError> {
        let aliases = Aliases::from_pairs(vec![("coder".into(), "real:1".into())]);
        parse_chat(body.to_string().as_bytes(), &aliases)
    }

    #[test]
    fn translates_every_supported_field() {
        let r = parse(json!({
            "model": "coder",
            "messages": [
                {"role": "developer", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "hi "}, {"type": "text", "text": "there"}]}
            ],
            "temperature": 0.2, "top_p": 0.9, "seed": 7, "max_completion_tokens": 64,
            "presence_penalty": 0.1, "frequency_penalty": 0.3, "stop": "END",
            "response_format": {"type": "json_object"},
            "stream": true, "stream_options": {"include_usage": true},
            "logit_bias": {"1": 2}, "user": "ignored"
        }))
        .unwrap();
        assert_eq!(r.model, "coder");
        assert_eq!(r.target, "real:1");
        let o = &r.ollama;
        assert_eq!(o["model"], "real:1");
        assert_eq!(
            o["messages"][0],
            json!({"role": "system", "content": "be brief"})
        );
        assert_eq!(o["messages"][1]["content"], "hi there");
        assert_eq!(
            o["options"],
            json!({"temperature": 0.2, "top_p": 0.9, "seed": 7, "num_predict": 64,
                   "presence_penalty": 0.1, "frequency_penalty": 0.3, "stop": ["END"]})
        );
        assert_eq!(o["format"], "json");
        assert_eq!(o["stream"], true);
        assert_eq!(o["evict_on_cancel"], false);
        assert_eq!(o["pin_load_options"], true);
        assert!(r.stream && r.include_usage);
        assert!(o.get("logit_bias").is_none() && o.get("user").is_none());
    }

    #[test]
    fn json_schema_response_format_passes_the_schema() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let r = parse(json!({"model": "m", "messages": [{"role": "user", "content": "x"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "s", "schema": schema}}}))
        .unwrap();
        assert_eq!(r.ollama["format"], schema);
        assert!(parse(
            json!({"model": "m", "messages": [{"role": "user", "content": "x"}],
            "response_format": {"type": "json_schema"}})
        )
        .is_err());
    }

    #[test]
    fn tools_pass_through_and_tool_choice_none_drops_them() {
        let tools =
            json!([{"type": "function", "function": {"name": "read_file", "parameters": {}}}]);
        let r = parse(json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": tools, "tool_choice": "auto"})).unwrap();
        assert_eq!(r.ollama["tools"], tools);
        assert_eq!(r.tool_names, vec!["read_file"]);
        let r = parse(json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": tools, "tool_choice": "none"})).unwrap();
        assert!(r.ollama.get("tools").is_none());
        assert!(r.tool_names.is_empty());
    }

    #[test]
    fn assistant_tool_calls_and_tool_results_translate_to_ollama() {
        let r = parse(json!({"model": "m", "messages": [
            {"role": "user", "content": "read it"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"}
        ]}))
        .unwrap();
        let m = &r.ollama["messages"];
        assert_eq!(
            m[1]["tool_calls"][0],
            json!({"function": {"name": "read_file", "arguments": {"path": "a.rs"}}})
        );
        assert_eq!(m[1]["content"], "");
        assert_eq!(
            m[2],
            json!({"role": "tool", "content": "fn main() {}", "tool_name": "read_file"})
        );
    }

    #[test]
    fn ollama_tool_calls_become_openai_calls_with_string_arguments() {
        let calls = tool_calls_to_openai(
            &[
                json!({"id": "abc", "function": {"index": 0, "name": "read_file", "arguments": {"path": "a.rs"}}}),
                json!({"function": {"name": "ls", "arguments": {}}}),
            ],
            0,
        );
        assert_eq!(calls[0]["id"], "abc");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(calls[1]["id"], "call_1", "id minted when Ollama omits it");
        assert_eq!(calls[1]["index"], 1);
    }

    #[test]
    fn finish_reason_and_usage() {
        assert_eq!(finish_reason(Some("stop"), true), "tool_calls");
        assert_eq!(finish_reason(Some("length"), false), "length");
        assert_eq!(finish_reason(None, false), "stop");
        assert_eq!(usage(5, 3)["total_tokens"], 8);
    }

    #[test]
    fn rejects_bad_requests_naming_the_param() {
        let p = |b: Value| parse(b).unwrap_err().param;
        assert_eq!(
            p(json!({"messages": [{"role": "user", "content": "x"}]})).as_deref(),
            Some("model")
        );
        assert_eq!(
            p(json!({"model": "m", "messages": []})).as_deref(),
            Some("messages")
        );
        assert_eq!(
            p(json!({"model": "m", "n": 2, "messages": [{"role": "user", "content": "x"}]}))
                .as_deref(),
            Some("n")
        );
        assert_eq!(p(json!({"model": "m", "messages": [{"role": "user", "content": [{"type": "image_url"}]}]})).as_deref(), Some("messages"));
        assert_eq!(
            p(json!({"model": "m", "stop": 3, "messages": [{"role": "user", "content": "x"}]}))
                .as_deref(),
            Some("stop")
        );
    }
}
