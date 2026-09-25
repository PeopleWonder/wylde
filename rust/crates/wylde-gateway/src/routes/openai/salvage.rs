//! Tool-call salvage for `/v1/chat/completions`.
//!
//! Some models (qwen2.5-coder, for one) write a tool call as text in the
//! reply (fenced JSON, a `<tools>` wrapper, or bare JSON) instead of as a
//! structured call. When a request offered `tools` and the reply carries
//! no structured calls, the harness salvage parser
//! (`wylde_harness::turn::salvage`) recovers text-form calls **for the
//! offered tool names only** and turns them into real `tool_calls`. If
//! nothing is recovered the reply text is returned untouched.
//!
//! Per-model setting: `WYLDE_OPENAI_TOOL_SALVAGE` is a comma-separated list
//! of model ids, where a trailing `*` matches a prefix. Unset or `*` means
//! every model; `off` disables salvage.

use std::collections::HashMap;

use serde_json::{json, Value};
use wylde_harness::turn::salvage::extract_tool_calls_from_content;

/// Env var holding the per-model salvage setting.
pub const SALVAGE_ENV: &str = "WYLDE_OPENAI_TOOL_SALVAGE";

/// Which models get tool-call salvage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SalvagePolicy(Vec<String>);

impl SalvagePolicy {
    pub fn parse(spec: &str) -> Self {
        let spec = spec.trim();
        if spec.is_empty() {
            return Self(vec!["*".to_owned()]);
        }
        if spec.eq_ignore_ascii_case("off") {
            return Self(Vec::new());
        }
        Self(
            spec.split(',')
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect(),
        )
    }

    pub fn from_env() -> Self {
        Self::parse(&std::env::var(SALVAGE_ENV).unwrap_or_default())
    }

    pub fn enabled_for(&self, model: &str) -> bool {
        self.0.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => p == model,
        })
    }
}

/// Recover tool calls written as text. Returns the reply text with the
/// calls scrubbed and the calls in Ollama's shape
/// (`{function: {name, arguments}}`), or `None` when nothing was recovered.
pub fn salvage(text: &str, tool_names: &[String]) -> Option<(String, Vec<Value>)> {
    let aliases: HashMap<String, String> =
        tool_names.iter().map(|n| (n.clone(), n.clone())).collect();
    let result = extract_tool_calls_from_content(text, &aliases);
    if result.recovered.is_empty() {
        return None;
    }
    let calls = result
        .recovered
        .iter()
        .map(|c| json!({"function": {"name": c.name, "arguments": c.args}}))
        .collect();
    // A `<tools>` wrapper isn't one of the parser's tag forms (its bare-JSON
    // pass finds the call inside); drop the empty tags it leaves behind.
    let cleaned = result
        .cleaned_text
        .replace("<tools>", "")
        .replace("</tools>", "")
        .trim()
        .to_owned();
    Some((cleaned, calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<String> {
        vec!["read_file".to_owned()]
    }

    #[test]
    fn policy_parsing() {
        assert!(SalvagePolicy::parse("").enabled_for("anything"));
        assert!(!SalvagePolicy::parse("off").enabled_for("anything"));
        let p = SalvagePolicy::parse("qwen2.5-coder*, exact:1");
        assert!(p.enabled_for("qwen2.5-coder:14b"));
        assert!(p.enabled_for("exact:1"));
        assert!(!p.enabled_for("exact:2"));
        assert!(!p.enabled_for("gemma4:12b"));
    }

    #[test]
    fn recovers_fenced_json() {
        let text = "```json\n{\n  \"name\": \"read_file\",\n  \"arguments\": {\n    \"path\": \"src/main.rs\"\n  }\n}\n```";
        let (cleaned, calls) = salvage(text, &tools()).expect("recovered");
        assert_eq!(
            calls,
            vec![json!({"function": {"name": "read_file", "arguments": {"path": "src/main.rs"}}})]
        );
        assert_eq!(cleaned, "");
    }

    #[test]
    fn recovers_a_tools_wrapped_call_and_drops_the_tags() {
        let text = "<tools>\n{\n  \"name\": \"read_file\",\n  \"arguments\": {\"path\": \"a.rs\"}\n}\n</tools>";
        let (cleaned, calls) = salvage(text, &tools()).expect("recovered");
        assert_eq!(calls[0]["function"]["arguments"], json!({"path": "a.rs"}));
        assert_eq!(cleaned, "");
    }

    #[test]
    fn recovers_bare_json_and_keeps_surrounding_prose() {
        let text = "Let me look. {\"name\": \"read_file\", \"arguments\": {\"path\": \"b.rs\"}}";
        let (cleaned, calls) = salvage(text, &tools()).expect("recovered");
        assert_eq!(calls.len(), 1);
        assert_eq!(cleaned, "Let me look.");
    }

    #[test]
    fn leaves_plain_answers_and_unoffered_tools_alone() {
        assert!(salvage("The answer is 4.", &tools()).is_none());
        assert!(salvage("{\"weather\": \"sunny\", \"temp\": 72}", &tools()).is_none());
        let other = "{\"name\": \"delete_everything\", \"arguments\": {}}";
        assert!(
            salvage(other, &tools()).is_none(),
            "only offered tools are recovered"
        );
    }
}
