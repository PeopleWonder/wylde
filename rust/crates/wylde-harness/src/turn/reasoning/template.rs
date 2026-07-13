//! ReWOO-style `${stepid.output.path}` placeholder resolution (plan §3.4,
//! slice S3).
//!
//! A [`PlanStep`](wylde_reasoning_plan::PlanStep)'s `args_template` may
//! reference earlier step results: `${s1.output}` splices the whole
//! result; `${s1.output.entries.0.name}` drills in (dot segments map to a
//! JSON Pointer, so numeric segments index arrays). Resolution is
//! **guidance rendering, not authoritative arg construction** (OQ-3:
//! Plan-and-Execute) — the fast model still emits the real tool call, so
//! an unresolved placeholder is left verbatim (honest: the model sees
//! that the plan referenced something unavailable) rather than erroring.
//!
//! Rules:
//! * a string that is EXACTLY one placeholder resolves to the referenced
//!   [`Value`] (type-preserving splice);
//! * placeholders embedded in a longer string are replaced textually
//!   (strings raw, other values compact-serialised);
//! * objects/arrays resolve recursively;
//! * an unknown step id or dead path leaves the placeholder text as-is.

use std::collections::HashMap;

use serde_json::Value;

/// Resolve every placeholder in `template` against `results`
/// (step id → that step's recorded result).
pub fn resolve(template: &Value, results: &HashMap<String, Value>) -> Value {
    match template {
        Value::String(s) => resolve_string(s, results),
        Value::Array(items) => Value::Array(items.iter().map(|v| resolve(v, results)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve(v, results)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// One string: whole-string placeholders splice the Value; embedded ones
/// substitute textually.
fn resolve_string(s: &str, results: &HashMap<String, Value>) -> Value {
    // Whole-string form first — preserves the referenced value's type.
    if let Some(inner) = parse_placeholder(s) {
        if let Some(v) = lookup(&inner, results) {
            return v;
        }
        return Value::String(s.to_owned()); // unresolved: leave verbatim
    }

    // Embedded form: textual substitution, left to right.
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find('}') {
            Some(end) => {
                let token = &after[..=end];
                match parse_placeholder(token).and_then(|p| lookup(&p, results)) {
                    Some(Value::String(text)) => out.push_str(&text),
                    Some(v) => out.push_str(&v.to_string()),
                    None => out.push_str(token), // unresolved: leave verbatim
                }
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated `${` — literal from here on.
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Value::String(out)
}

/// A parsed `${sid.output(.path)*}` reference.
struct Placeholder {
    step_id: String,
    /// JSON Pointer into the step result (`""` = the whole result).
    pointer: String,
}

/// Parse a candidate token. `None` unless it has the exact
/// `${<id>.output[...]}` shape.
fn parse_placeholder(token: &str) -> Option<Placeholder> {
    let inner = token.strip_prefix("${")?.strip_suffix('}')?;
    let (step_id, tail) = match inner.split_once('.') {
        Some((id, tail)) => (id, tail),
        None => return None,
    };
    if step_id.is_empty() || step_id.contains(char::is_whitespace) {
        return None;
    }
    let path = if tail == "output" {
        ""
    } else {
        tail.strip_prefix("output.")?
    };
    let pointer = if path.is_empty() {
        String::new()
    } else {
        format!("/{}", path.replace('.', "/"))
    };
    Some(Placeholder {
        step_id: step_id.to_owned(),
        pointer,
    })
}

fn lookup(p: &Placeholder, results: &HashMap<String, Value>) -> Option<Value> {
    let root = results.get(&p.step_id)?;
    if p.pointer.is_empty() {
        return Some(root.clone());
    }
    root.pointer(&p.pointer).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn results() -> HashMap<String, Value> {
        HashMap::from([
            (
                "s1".to_owned(),
                json!({"entries": [{"name": "Cargo.toml"}, {"name": "src"}], "count": 2}),
            ),
            ("s2".to_owned(), json!("plain text result")),
        ])
    }

    #[test]
    fn whole_string_placeholder_splices_the_value() {
        let t = json!({"query": "${s1.output}"});
        let r = resolve(&t, &results());
        assert_eq!(r["query"]["count"], 2, "type-preserving splice");
    }

    #[test]
    fn dotted_path_drills_in_including_array_indices() {
        let t = json!({"path": "${s1.output.entries.0.name}", "n": "${s1.output.count}"});
        let r = resolve(&t, &results());
        assert_eq!(r["path"], "Cargo.toml");
        assert_eq!(r["n"], 2, "numeric value splices as a number");
    }

    #[test]
    fn embedded_placeholder_substitutes_textually() {
        let t = json!({"q": "look in ${s1.output.entries.1.name} for ${s2.output}"});
        let r = resolve(&t, &results());
        assert_eq!(r["q"], "look in src for plain text result");
    }

    #[test]
    fn embedded_non_string_serialises_compactly() {
        let t = json!({"q": "entries: ${s1.output.entries}"});
        let r = resolve(&t, &results());
        assert_eq!(
            r["q"],
            "entries: [{\"name\":\"Cargo.toml\"},{\"name\":\"src\"}]"
        );
    }

    #[test]
    fn unresolved_placeholders_stay_verbatim() {
        let t = json!({
            "whole": "${s9.output}",
            "path": "${s1.output.missing.deep}",
            "embedded": "x ${s9.output.y} z",
        });
        let r = resolve(&t, &results());
        assert_eq!(r["whole"], "${s9.output}");
        assert_eq!(r["path"], "${s1.output.missing.deep}");
        assert_eq!(r["embedded"], "x ${s9.output.y} z");
    }

    #[test]
    fn non_placeholder_strings_and_scalars_pass_through() {
        let t = json!({"a": "just text", "b": 7, "c": true, "d": null, "e": ["${s2.output}"]});
        let r = resolve(&t, &results());
        assert_eq!(r["a"], "just text");
        assert_eq!(r["b"], 7);
        assert_eq!(r["e"][0], "plain text result");
    }

    #[test]
    fn malformed_tokens_are_literal() {
        // No `.output` segment, unterminated brace, bare `${}` — all literal.
        let t = json!({"a": "${s1}", "b": "tail ${s1.output", "c": "${}"});
        let r = resolve(&t, &results());
        assert_eq!(r["a"], "${s1}");
        assert_eq!(r["b"], "tail ${s1.output");
        assert_eq!(r["c"], "${}");
    }
}
