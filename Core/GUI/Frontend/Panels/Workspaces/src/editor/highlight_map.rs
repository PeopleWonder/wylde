//! Map `treesitter.highlight` scopes → editor decorations (IDE S4).
//!
//! The tree-sitter sidecar returns spans tagged with capture **scope** names
//! (`"keyword"`, `"function.method"`, `"string"`, `"comment"`, …). This module
//! owns the scope→theme-token mapping (the panel is where the tree-sitter
//! naming coupling lives; the colours themselves are theme tokens, never
//! hardcoded). It is pure so the mapping is unit-testable without a window.

use gpui::Rgba;
use serde_json::Value;
use wylde_gpui_code_editor::Decoration;
use wylde_theme::colors::syntax;

/// Resolve a tree-sitter scope to a foreground colour, matching on the scope's
/// dotted prefix (`"function.method"` → `function`). Returns `None` for scopes
/// we don't theme (the editor leaves those as default `TEXT_PRIMARY`).
pub fn color_for_scope(scope: &str) -> Option<Rgba> {
    // The most-specific-first prefix the scope starts with wins.
    let root = scope.split('.').next().unwrap_or(scope);
    let c = match root {
        "keyword" | "conditional" | "repeat" | "include" | "storageclass" => syntax::KEYWORD,
        "string" | "char" => syntax::STRING,
        "comment" => syntax::COMMENT,
        "function" | "method" | "constructor" => syntax::FUNCTION,
        "type" | "class" | "struct" | "interface" | "enum" | "namespace" | "module" => syntax::TYPE,
        "number" | "float" | "boolean" => syntax::NUMBER,
        "constant" => syntax::CONSTANT,
        "variable" | "property" | "field" | "parameter" => syntax::VARIABLE,
        "operator" => syntax::OPERATOR,
        "punctuation" | "delimiter" | "bracket" => syntax::PUNCTUATION,
        "attribute" | "annotation" | "decorator" => syntax::ATTRIBUTE,
        "tag" | "label" | "tag.error" => syntax::TAG,
        _ => return None,
    };
    Some(c)
}

/// Convert a `treesitter.highlight` reply (`{spans:[{start_byte, end_byte,
/// scope}]}`) into editor decorations. Spans whose scope we don't theme are
/// dropped (they render as default text). Out-of-order or zero-width spans are
/// tolerated; the editor clamps ranges to the buffer at paint.
pub fn decorations_from_reply(reply: &Value) -> Vec<Decoration> {
    let Some(spans) = reply.get("spans").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(spans.len());
    for s in spans {
        let (Some(start), Some(end), Some(scope)) = (
            s.get("start_byte").and_then(Value::as_u64),
            s.get("end_byte").and_then(Value::as_u64),
            s.get("scope").and_then(Value::as_str),
        ) else {
            continue;
        };
        if end <= start {
            continue;
        }
        if let Some(color) = color_for_scope(scope) {
            out.push(Decoration::color(start as usize..end as usize, color));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prefix_match_resolves_dotted_scopes() {
        assert_eq!(color_for_scope("function.method"), Some(syntax::FUNCTION));
        assert_eq!(
            color_for_scope("punctuation.bracket"),
            Some(syntax::PUNCTUATION)
        );
        assert_eq!(color_for_scope("keyword"), Some(syntax::KEYWORD));
        assert!(color_for_scope("something.unthemed").is_none());
    }

    #[test]
    fn reply_maps_to_decorations_and_drops_unthemed() {
        let reply = json!({
            "spans": [
                { "start_byte": 0, "end_byte": 2, "scope": "keyword" },
                { "start_byte": 3, "end_byte": 7, "scope": "function" },
                { "start_byte": 8, "end_byte": 9, "scope": "totally.unknown" },
                { "start_byte": 9, "end_byte": 9, "scope": "keyword" }, // zero-width
            ]
        });
        let decos = decorations_from_reply(&reply);
        assert_eq!(decos.len(), 2, "unthemed + zero-width dropped");
        assert_eq!(decos[0].range, 0..2);
        assert_eq!(decos[1].range, 3..7);
    }

    #[test]
    fn missing_spans_is_empty() {
        assert!(decorations_from_reply(&json!({})).is_empty());
    }
}
