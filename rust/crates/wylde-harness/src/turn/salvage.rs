//! Assistant-content tool-call salvage parser — Rust port of
//! `Core/harness/turn/_streaming.py:255-373`.
//!
//! The model sometimes emits its tool call as plain text in the
//! assistant `content` field rather than the structured `tool_calls`
//! channel. This violates the architectural rule that tool calls live
//! on `chat.stream_tools` and user-visible content lives on
//! `chat.stream_turn`. The salvage parser detects three common shapes,
//! extracts the calls, and scrubs them from the content so the chat
//! bubble never renders raw JSON.
//!
//! Detection priority (highest first):
//!
//! 1. Fenced JSON — ```` ```json {...} ``` ````
//! 2. Tag-wrapped — `<tool_call>...</tool_call>`, `<function_call>...`,
//!    `<tool_use>...`
//! 3. Bare balanced-brace JSON — a structural guard (object carries a
//!    `name`/`function` key, or is a single-key object) keeps prose
//!    JSON like `{"weather": "sunny", "temp": 72}` from being scrubbed.
//!
//! Two malformed shapes smaller models emit are recovered via a bounded
//! single-level unwrap (see [`unwrap_single_key`]): single-key wrapper
//! objects (`{"tool_search": {"name": ...}}`) and dotted-flattened keys
//! (`{"search.file_list.path": "."}`).
//!
//! Parity with the Python implementation is enforced by mirroring the
//! same byte sequences the Python test_streaming.py uses (see
//! `tests/salvage_parity.rs`).

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// One tool call recovered from the assistant content (resolved against
/// the alias map). Mirrors the Python dict `{id, name, args, raw_name}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub raw_name: String,
}

/// One tool call parsed cleanly but whose name did NOT resolve to a
/// known tool. Mirrors the Python dict `{id, name, args}`. Callers fire
/// a `tool_error` with reason `tool_call_text_unrecognised` and still
/// scrub the JSON from the cleaned text so the chat bubble doesn't
/// render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrecognisedCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Output of [`extract_tool_calls_from_content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SalvageResult {
    pub cleaned_text: String,
    pub recovered: Vec<RecoveredCall>,
    pub unrecognised: Vec<UnrecognisedCall>,
}

fn fenced_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?s)```(?:json)?\s*(\{.*?\})\s*```").expect("fenced re"))
}

fn tag_patterns() -> &'static [Regex] {
    static R: OnceLock<Vec<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        vec![
            Regex::new(r"(?s)<tool_call>\s*(.*?)\s*</tool_call>").expect("tool_call re"),
            Regex::new(r"(?s)<function_call>\s*(.*?)\s*</function_call>")
                .expect("function_call re"),
            Regex::new(r"(?s)<tool_use>\s*(.*?)\s*</tool_use>").expect("tool_use re"),
        ]
    })
}

/// Yield half-open `(start, end)` byte spans for every top-level
/// balanced `{...}` object in `text`.
///
/// Respects double-quoted strings (no brace counting inside `"..."`)
/// and backslash escapes within strings. Skips fragments whose braces
/// don't balance.
///
/// Byte-indexed so the caller can splice the original `text` directly.
/// Inputs the parser sees are always ASCII JSON, so multi-byte chars in
/// surrounding prose are safe — brace/quote detection only touches
/// single-byte ASCII.
pub fn find_balanced_braces(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let mut depth: i32 = 0;
        let mut in_str = false;
        let mut esc = false;
        let mut j = i;
        let mut found_end = false;
        while j < n {
            let c = bytes[j];
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    out.push((i, j + 1));
                    i = j + 1;
                    found_end = true;
                    break;
                }
            }
            j += 1;
        }
        if !found_end {
            return out;
        }
    }
    out
}

/// Coerce one parsed JSON object into `{name, args}` if it looks like a
/// tool call, else return `None`.
///
/// Accepts both `{"name": ..., "arguments": ...}` (Ollama/Qwen) and
/// `{"name": ..., "parameters": ...}` (Llama) and the nested
/// `{"function": {"name": ..., "arguments": ...}}` form. `arguments`
/// that came through as a string get re-parsed as JSON.
pub fn parse_one_call(obj: &Value) -> Option<(String, Value)> {
    let map = obj.as_object()?;

    // Resolve name — top-level `name` first, then `function.name`.
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            map.get("function")
                .and_then(Value::as_object)
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let name = match name {
        Some(s) if !s.is_empty() => s,
        _ => return None,
    };

    // Resolve args — `arguments` | `parameters` | `function.arguments` | {}.
    let raw_args = map
        .get("arguments")
        .cloned()
        .or_else(|| map.get("parameters").cloned())
        .or_else(|| {
            map.get("function")
                .and_then(Value::as_object)
                .and_then(|f| f.get("arguments").cloned())
        })
        .unwrap_or_else(|| Value::Object(Map::new()));

    let args = coerce_args(raw_args);
    Some((name, args))
}

/// Attempt to recover a tool call from a single-key object that
/// [`parse_one_call`] doesn't directly understand. Handles the two
/// malformed shapes observed from small models (e.g. qwen2.5:0.5b),
/// bounded to a single level of unwrap:
///
/// 1. **Single-key wrapper** — `{"tool_search": {"name": "time.now",
///    "arguments": {}}}`. The outer key is a non-canonical wrapper name
///    (`tool_call`, `tool_search`, `function_call`, `call`, `action`,
///    `tool`, …); the value is itself an object. Unwrap one level and
///    parse the inner object as a tool call. The unwrap calls
///    [`parse_one_call`] (NOT itself) on the inner object, so a
///    double-wrap stops here rather than recursing.
/// 2. **Dotted flattened key** — `{"search.file_list.path": "."}`. The
///    single key is `<tool_name>.<arg_name>` and the value is the arg
///    value. Tool names themselves contain dots, so the split point is
///    disambiguated against `alias_map`: split on the last `.`, and
///    accept only when the prefix resolves to a known tool. Reconstruct
///    as `{"name": "search.file_list", "arguments": {"path": "."}}`.
///
/// Returns `None` for anything else (multi-key objects, unknown
/// prefixes, non-object wrapper values) so callers stay conservative.
fn unwrap_single_key(
    obj: &Value,
    alias_map: &std::collections::HashMap<String, String>,
) -> Option<(String, Value)> {
    let map = obj.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let (key, value) = map.iter().next()?;

    // Shape 1 — wrapper object. Unwrap one level and parse the inner
    // object directly. Bounded: parse_one_call does not recurse, so a
    // double-wrap (`{"a": {"b": {"name": ...}}}`) yields None here.
    if value.is_object() {
        if let Some(call) = parse_one_call(value) {
            return Some(call);
        }
    }

    // Shape 2 — dotted flattened key. Arg names don't contain dots, so
    // the tool/arg boundary is the last `.`; the prefix must resolve to
    // a known tool in the alias map (canonical id, dotted, or snake
    // form) for the split to be accepted.
    if let Some(pos) = key.rfind('.') {
        let tool = &key[..pos];
        let arg = &key[pos + 1..];
        if !tool.is_empty() && !arg.is_empty() && alias_map.contains_key(tool) {
            let mut args = Map::new();
            args.insert(arg.to_string(), value.clone());
            return Some((tool.to_string(), Value::Object(args)));
        }
    }

    None
}

/// String → JSON re-parse + dict-or-`{_raw}` fallback. Matches Python's
/// `_parse_one_call` args-shape coercion exactly.
fn coerce_args(v: Value) -> Value {
    let v = if let Value::String(s) = &v {
        match serde_json::from_str::<Value>(s) {
            Ok(parsed) => parsed,
            Err(_) => {
                let mut m = Map::new();
                m.insert("_raw".to_string(), Value::String(s.clone()));
                return Value::Object(m);
            }
        }
    } else {
        v
    };
    if matches!(v, Value::Object(_)) {
        v
    } else {
        let mut m = Map::new();
        m.insert("_raw".to_string(), v);
        Value::Object(m)
    }
}

/// Find and excise tool-call shapes from assistant content.
///
/// Detection priority is fenced JSON → tag-wrapped → bare JSON. Bare
/// JSON requires a `"name"` substring to avoid false positives on
/// prose JSON like `{"weather": "sunny"}`.
///
/// `alias_map` resolves the model-emitted name (which may be dotted,
/// snake-cased, or a manifest name) to a canonical tool id. Names not
/// in the map land in `unrecognised`; the JSON is still scrubbed.
pub fn extract_tool_calls_from_content(
    text: &str,
    alias_map: &std::collections::HashMap<String, String>,
) -> SalvageResult {
    if text.is_empty() {
        return SalvageResult {
            cleaned_text: String::new(),
            recovered: Vec::new(),
            unrecognised: Vec::new(),
        };
    }

    let mut recovered: Vec<RecoveredCall> = Vec::new();
    let mut unrecognised: Vec<UnrecognisedCall> = Vec::new();
    let mut seq: u32 = 0;

    let mut consume = |parsed: &Value| -> bool {
        // Canonical `{name, arguments}` / `{function: {...}}` first;
        // fall back to the single-level unwrap for wrapper / dotted
        // shapes the small models emit.
        let call = parse_one_call(parsed).or_else(|| unwrap_single_key(parsed, alias_map));
        let Some((name, args)) = call else {
            return false;
        };
        seq += 1;
        let call_id = format!("call_text_{seq}");
        if let Some(canonical) = alias_map.get(&name) {
            recovered.push(RecoveredCall {
                id: call_id,
                name: canonical.clone(),
                args,
                raw_name: name,
            });
        } else {
            unrecognised.push(UnrecognisedCall {
                id: call_id,
                name,
                args,
            });
        }
        true
    };

    // Pass 1 — fenced ```json ...``` blocks. Replace with empty string
    // when body parses as a tool call; leave intact otherwise so a
    // user's fenced JSON example survives.
    let working = replace_regex(text, fenced_re(), |body| {
        match serde_json::from_str::<Value>(body) {
            Ok(obj) if consume(&obj) => Some(String::new()),
            _ => None,
        }
    });

    // Pass 2 — explicit tool-call tags.
    let mut working = working;
    for pat in tag_patterns().iter() {
        working = replace_regex(&working, pat, |body| {
            let body = body.trim();
            match serde_json::from_str::<Value>(body) {
                Ok(obj) if consume(&obj) => Some(String::new()),
                _ => None,
            }
        });
    }

    // Pass 3 — bare balanced-brace JSON. Each span is parsed and run
    // through a cheap structural guard before `consume` so prose JSON
    // (`{"weather": "sunny", "temp": 72}`) is never scrubbed: an object
    // is a tool-call candidate only if it carries a `name`/`function`
    // key (canonical or wrapper shapes) OR it is a single-key object
    // (the wrapper / dotted-flattened shapes `unwrap_single_key`
    // handles). `consume` itself is conservative — it returns false
    // (leaving the span intact) for anything that doesn't resolve.
    let spans = find_balanced_braces(&working);
    let mut spans_to_remove: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        let span = &working[start..end];
        let obj: Value = match serde_json::from_str(span) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Value::Object(m) = &obj else { continue };
        let looks_like_call = m.contains_key("name") || m.contains_key("function") || m.len() == 1;
        if !looks_like_call {
            continue;
        }
        if consume(&obj) {
            spans_to_remove.push((start, end));
        }
    }

    // Strip right-to-left so earlier offsets stay valid.
    spans_to_remove.sort_by(|a, b| b.0.cmp(&a.0));
    let mut cleaned = working;
    for (start, end) in spans_to_remove {
        cleaned.replace_range(start..end, "");
    }

    SalvageResult {
        cleaned_text: cleaned.trim().to_string(),
        recovered,
        unrecognised,
    }
}

/// Like `regex::Regex::replace_all` but invokes `f(captured_body)` per
/// match: `Some(replacement)` substitutes, `None` leaves the match
/// intact (mirrors the Python `_fenced_sub` / `_tag_sub` pattern of
/// returning `m.group(0)` for "don't substitute").
fn replace_regex<F>(text: &str, re: &Regex, mut f: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for caps in re.captures_iter(text) {
        let m = caps.get(0).expect("0 group");
        let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        out.push_str(&text[last..m.start()]);
        match f(body) {
            Some(replacement) => out.push_str(&replacement),
            None => out.push_str(m.as_str()),
        }
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Stable per-turn dedupe key over `(name, args)`. Args canonicalised
/// with sorted keys so equivalent payloads in different iteration
/// orders hash the same.
pub fn call_hash(name: &str, args: &Value) -> String {
    let canonical = canonicalise_for_hash(args);
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(b"\x00");
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn canonicalise_for_hash(v: &Value) -> String {
    // `serde_json::to_string` does NOT sort keys; mirror Python's
    // `json.dumps(..., sort_keys=True)` by walking and sorting maps.
    fn rec(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => {
                let escaped = serde_json::Value::String(s.clone()).to_string();
                out.push_str(&escaped);
            }
            Value::Array(a) => {
                out.push('[');
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    rec(item, out);
                }
                out.push(']');
            }
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let key_s = serde_json::Value::String((*k).clone()).to_string();
                    out.push_str(&key_s);
                    out.push_str(": ");
                    rec(&m[*k], out);
                }
                out.push('}');
            }
        }
    }
    let mut s = String::new();
    rec(v, &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn alias(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── Parity tests against Python's test_streaming.py byte sequences ──

    #[test]
    fn extract_bare_json_tool_call_recovered() {
        let map = alias(&[
            ("memory_long_term_save", "memory_long_term_save"),
            ("memory.long_term.save", "memory_long_term_save"),
        ]);
        let text = r#"{"name": "memory.long_term.save", "arguments": {"body": "kebab"}}"#;
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.cleaned_text, "");
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(r.recovered[0].name, "memory_long_term_save");
        assert_eq!(r.recovered[0].raw_name, "memory.long_term.save");
        assert_eq!(r.recovered[0].args, serde_json::json!({"body": "kebab"}));
        assert!(r.unrecognised.is_empty());
    }

    #[test]
    fn extract_tag_wrapped_recovered() {
        let map = alias(&[("git_status", "git_status")]);
        let text = "I'll check that.\n<tool_call>{\"name\": \"git_status\", \"arguments\": {}}</tool_call>\nDone.";
        let r = extract_tool_calls_from_content(text, &map);
        assert!(!r.cleaned_text.contains("git_status"));
        assert!(r.cleaned_text.contains("I'll check that."));
        assert!(r.cleaned_text.contains("Done."));
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(r.recovered[0].name, "git_status");
        assert_eq!(r.recovered[0].args, serde_json::json!({}));
    }

    #[test]
    fn extract_fenced_json_recovered() {
        let map = alias(&[("rag_ask", "rag_ask")]);
        let text =
            "Here you go:\n```json\n{\"name\": \"rag_ask\", \"arguments\": {\"q\": \"test\"}}\n```\n";
        let r = extract_tool_calls_from_content(text, &map);
        assert!(!r.cleaned_text.contains("```"));
        assert!(!r.cleaned_text.contains("rag_ask"));
        assert!(r.cleaned_text.contains("Here you go:"));
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(r.recovered[0].name, "rag_ask");
        assert_eq!(r.recovered[0].args, serde_json::json!({"q": "test"}));
    }

    #[test]
    fn extract_unrecognised_name() {
        let map = alias(&[("git_status", "git_status")]);
        let text = r#"{"name": "nonexistent_tool", "arguments": {"x": 1}}"#;
        let r = extract_tool_calls_from_content(text, &map);
        // JSON still scrubbed; lands in unrecognised.
        assert_eq!(r.cleaned_text, "");
        assert!(r.recovered.is_empty());
        assert_eq!(r.unrecognised.len(), 1);
        assert_eq!(r.unrecognised[0].name, "nonexistent_tool");
        assert_eq!(r.unrecognised[0].args, serde_json::json!({"x": 1}));
    }

    #[test]
    fn extract_mixed_prose_and_tool_call() {
        let map = alias(&[("git_diff", "git_diff")]);
        let text = r#"Let me check the diff for you. {"name": "git_diff", "arguments": {"path": "."}} Be right back!"#;
        let r = extract_tool_calls_from_content(text, &map);
        assert!(!r.cleaned_text.contains("git_diff"));
        assert!(r.cleaned_text.contains("Let me check the diff for you."));
        assert!(r.cleaned_text.contains("Be right back!"));
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(r.recovered[0].name, "git_diff");
    }

    #[test]
    fn extract_does_not_falsepositive_on_prose_json() {
        let map = alias(&[("git_status", "git_status")]);
        let text = r#"The forecast is {"weather": "sunny", "temp": 72}. Nothing else to report."#;
        let r = extract_tool_calls_from_content(text, &map);
        // No `"name"` substring → never even parsed; stays in cleaned.
        assert_eq!(r.cleaned_text, text.trim());
        assert!(r.recovered.is_empty());
        assert!(r.unrecognised.is_empty());
    }

    #[test]
    fn find_balanced_braces_respects_strings_and_escapes() {
        let s = r#"prelude {"a":{"b":"}escaped"}, "c":1} tail"#;
        let spans = find_balanced_braces(s);
        assert_eq!(spans.len(), 1, "spans: {spans:?}");
        let (start, end) = spans[0];
        let span = &s[start..end];
        assert!(span.starts_with('{'));
        assert!(span.ends_with('}'));
        // Round-trip parses as JSON.
        let v: Value = serde_json::from_str(span).expect("parses");
        assert_eq!(v["a"]["b"], "}escaped");
        assert_eq!(v["c"], 1);
    }

    #[test]
    fn find_balanced_braces_skips_unbalanced_trailer() {
        let s = r#"{"a": 1} and then {"b": 2"#;
        let spans = find_balanced_braces(s);
        assert_eq!(spans.len(), 1);
        let span = &s[spans[0].0..spans[0].1];
        assert_eq!(span, r#"{"a": 1}"#);
    }

    #[test]
    fn parse_one_call_accepts_nested_function() {
        let v = serde_json::json!({"function": {"name": "x", "arguments": {"k": 1}}});
        let (name, args) = parse_one_call(&v).expect("parses");
        assert_eq!(name, "x");
        assert_eq!(args, serde_json::json!({"k": 1}));
    }

    #[test]
    fn parse_one_call_treats_string_arguments_as_json() {
        let v = serde_json::json!({"name": "x", "arguments": "{\"k\": 1}"});
        let (_, args) = parse_one_call(&v).expect("parses");
        assert_eq!(args, serde_json::json!({"k": 1}));
    }

    #[test]
    fn parse_one_call_wraps_non_dict_args_in_raw() {
        let v = serde_json::json!({"name": "x", "arguments": "not json at all"});
        let (_, args) = parse_one_call(&v).expect("parses");
        assert_eq!(args, serde_json::json!({"_raw": "not json at all"}));
    }

    #[test]
    fn parse_one_call_rejects_missing_name() {
        let v = serde_json::json!({"arguments": {"x": 1}});
        assert!(parse_one_call(&v).is_none());
    }

    #[test]
    fn call_hash_is_order_insensitive_over_args() {
        let a = serde_json::json!({"a": 1, "b": 2});
        let b = serde_json::json!({"b": 2, "a": 1});
        assert_eq!(call_hash("tool", &a), call_hash("tool", &b));
    }

    #[test]
    fn call_hash_distinguishes_name_changes() {
        let v = serde_json::json!({"a": 1});
        assert_ne!(call_hash("tool_a", &v), call_hash("tool_b", &v));
    }

    #[test]
    fn extract_two_text_emissions_recovers_both() {
        // Mirrors test_dedupe_two_text_emissions_same_call's input: the
        // salvage parser should extract BOTH copies; dedupe lives in
        // tool_round.rs.
        let map = alias(&[("git_status", "git_status")]);
        let text = "{\"name\": \"git_status\", \"arguments\": {}}\n{\"name\": \"git_status\", \"arguments\": {}}";
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 2);
        assert!(r.cleaned_text.is_empty());
    }

    #[test]
    fn extract_empty_input_is_empty() {
        let map: HashMap<String, String> = HashMap::new();
        let r = extract_tool_calls_from_content("", &map);
        assert!(r.cleaned_text.is_empty());
        assert!(r.recovered.is_empty());
        assert!(r.unrecognised.is_empty());
    }

    #[test]
    fn extract_fenced_without_json_marker_still_matches() {
        // Python regex: ```(?:json)?\s*({...})\s*``` — the `json`
        // marker is optional.
        let map = alias(&[("foo", "foo")]);
        let text = "```\n{\"name\": \"foo\", \"arguments\": {}}\n```";
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1);
    }

    #[test]
    fn extract_fenced_with_unparseable_body_stays_intact() {
        let map = alias(&[("foo", "foo")]);
        let text = "```json\nnot valid json\n```";
        let r = extract_tool_calls_from_content(text, &map);
        assert!(r.cleaned_text.contains("not valid json"));
        assert!(r.recovered.is_empty());
    }

    #[test]
    fn extract_function_call_tag_alias_works() {
        let map = alias(&[("foo", "foo")]);
        let text = "<function_call>{\"name\": \"foo\", \"arguments\": {}}</function_call>";
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1);
    }

    #[test]
    fn extract_tool_use_tag_alias_works() {
        let map = alias(&[("foo", "foo")]);
        let text = "<tool_use>{\"name\": \"foo\", \"arguments\": {}}</tool_use>";
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1);
    }

    #[test]
    fn extract_bare_with_parameters_field_works() {
        // Llama uses "parameters" instead of "arguments".
        let map = alias(&[("foo", "foo")]);
        let text = r#"{"name": "foo", "parameters": {"k": 1}}"#;
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(r.recovered[0].args, serde_json::json!({"k": 1}));
    }

    // ── Fix A: single-key wrapper + dotted-flattened unwrap ──────────────

    #[test]
    fn unwrap_canonical_happy_path_unchanged() {
        // A well-formed `{name, arguments}` object never hits the
        // unwrap path — parse_one_call handles it directly.
        let map = alias(&[("time.now", "time_now")]);
        let text = r#"{"name": "time.now", "arguments": {}}"#;
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(r.recovered[0].name, "time_now");
        assert_eq!(r.recovered[0].raw_name, "time.now");
        assert_eq!(r.cleaned_text, "");
    }

    #[test]
    fn unwrap_single_key_wrapper_recovers() {
        // qwen2.5:0.5b shape: tool call buried under a wrapper key.
        let map = alias(&[("time.now", "time_now")]);
        let text = r#"{"tool_search": {"name": "time.now", "arguments": {}}}"#;
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1, "recovered: {:?}", r.recovered);
        assert_eq!(r.recovered[0].name, "time_now");
        assert_eq!(r.recovered[0].raw_name, "time.now");
        assert_eq!(r.recovered[0].args, serde_json::json!({}));
        assert_eq!(r.cleaned_text, "");
    }

    #[test]
    fn unwrap_dotted_flattened_key_recovers() {
        // Flattened `<tool>.<arg>: value` shape. The tool name itself
        // contains a dot, so the split is disambiguated against the
        // alias map (last dot, prefix must be a known tool).
        let map = alias(&[
            ("search.file_list", "file_list"),
            ("search_file_list", "file_list"),
        ]);
        let text = r#"{"search.file_list.path": "."}"#;
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.recovered.len(), 1, "recovered: {:?}", r.recovered);
        assert_eq!(r.recovered[0].name, "file_list");
        assert_eq!(r.recovered[0].raw_name, "search.file_list");
        assert_eq!(r.recovered[0].args, serde_json::json!({"path": "."}));
        assert_eq!(r.cleaned_text, "");
    }

    #[test]
    fn unwrap_double_wrap_stops_at_one_level() {
        // Two layers of wrapper — the inner object is itself a wrapper,
        // not a `{name, arguments}` call. The bounded unwrap parses one
        // level then gives up, so nothing is recovered.
        let map = alias(&[("time.now", "time_now")]);
        let inner = serde_json::json!({"tool_call": {"name": "time.now", "arguments": {}}});
        let double = serde_json::json!({"wrapper": inner});
        assert!(
            unwrap_single_key(&double, &map).is_none(),
            "double-wrap must not recover"
        );
    }

    #[test]
    fn unwrap_bogus_shapes_return_no_call() {
        let map = alias(&[("search.file_list", "file_list")]);
        // Multi-key prose object — not single-key, no name/function.
        assert!(unwrap_single_key(&serde_json::json!({"a": 1, "b": 2}), &map).is_none());
        // Single-key wrapper whose value isn't a tool call.
        assert!(unwrap_single_key(&serde_json::json!({"weather": {"temp": 72}}), &map).is_none());
        // Dotted key whose prefix isn't a known tool.
        assert!(unwrap_single_key(&serde_json::json!({"unknown.path": "."}), &map).is_none());
        // Single-key scalar with no dot — nothing to unwrap.
        assert!(unwrap_single_key(&serde_json::json!({"answer": 42}), &map).is_none());
    }

    #[test]
    fn unwrap_does_not_scrub_single_key_prose() {
        // A single-key object that isn't a tool call must survive in
        // cleaned_text — consume() returns false, so the span stays.
        let map = alias(&[("git_status", "git_status")]);
        let text = r#"The result is {"answer": 42}. Done."#;
        let r = extract_tool_calls_from_content(text, &map);
        assert_eq!(r.cleaned_text, text.trim());
        assert!(r.recovered.is_empty());
        assert!(r.unrecognised.is_empty());
    }
}
