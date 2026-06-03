//! Tolerant JSON parsing — branch-for-branch port of the Python handler's
//! `_try_parse_json`.
//!
//! Local models like to wrap JSON in ```json``` fences or prepend an
//! explanation paragraph. We try a strict parse first, then fall back to the
//! longest brace-delimited substring (`\{.*\}` with DOTALL semantics). Returns
//! [`None`] when nothing parses — exactly what the Python returns, so the
//! summarize/explain/flashcards handlers emit `null` for their structured
//! fields on an unparseable model reply.

use serde_json::Value;

/// Best-effort JSON parse, lenient with markdown code fences and prose
/// preambles. Mirrors `handler.py::_try_parse_json`.
pub fn try_parse_json(text: &str) -> Option<Value> {
    // Strict parse first.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    // Fall back to the longest `{ ... }` span (Python's `re.DOTALL` regex
    // `\{.*\}` is greedy — it matches from the first `{` to the LAST `}`).
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let candidate = &text[start..=end];
    serde_json::from_str::<Value>(candidate).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_object_parses() {
        assert_eq!(
            try_parse_json(r#"{"summary": "hi"}"#),
            Some(json!({"summary": "hi"}))
        );
    }

    #[test]
    fn fenced_json_parses_via_fallback() {
        let raw = "Here you go:\n```json\n{\"a\": 1}\n```\nthanks";
        assert_eq!(try_parse_json(raw), Some(json!({"a": 1})));
    }

    #[test]
    fn prose_preamble_then_object() {
        let raw = "Sure! {\"explanation\": \"x\", \"analogy\": \"y\"}";
        assert_eq!(
            try_parse_json(raw),
            Some(json!({"explanation": "x", "analogy": "y"}))
        );
    }

    #[test]
    fn greedy_to_last_brace() {
        // Two objects in prose — Python's greedy `\{.*\}` spans both braces;
        // the inner text isn't valid JSON, so the whole span fails and we
        // return None (matching Python's behaviour exactly).
        let raw = "{\"a\": 1} and {\"b\": 2}";
        assert_eq!(try_parse_json(raw), None);
    }

    #[test]
    fn unparseable_returns_none() {
        assert_eq!(try_parse_json("no json here"), None);
        assert_eq!(try_parse_json(""), None);
    }
}
