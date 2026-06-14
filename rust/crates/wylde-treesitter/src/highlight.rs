//! Syntax-highlight spans — the `treesitter.highlight` verb (Slice H, plan
//! §"API surface" / slice 4 "IDE verbs").
//!
//! Input  `{path, language?}`
//! Output `{spans:[{start_byte, end_byte, scope}]}`
//!
//! Driven by the `tree-sitter-highlight` crate (the plan's named dependency)
//! over each grammar crate's OWN bundled `highlights.scm` — the canonical
//! query maintained alongside the grammar, so we never hand-mirror upstream.
//! `scope` is the query's capture name (`"function"`, `"keyword"`,
//! `"string"`, `"punctuation.bracket"`, …) — the consumer maps scopes to
//! colours (Theme-side concern; this verb is presentation-free).
//!
//! Combination rules follow upstream guidance: the TypeScript query layers on
//! top of JavaScript's; TSX additionally takes the JSX query. **No injections
//! in v1** — a Markdown code fence highlights as fence chrome, not as its
//! inner language (single-language passes keep the verb pure per-file; an
//! injection pass is a follow-up if a consumer wants it).
//!
//! Spans are byte ranges into the file, half-open (`end_byte` exclusive).
//! Unhighlighted stretches are simply absent. Payloads stay KB-sized — span
//! metadata only, never source bytes.

use serde_json::{json, Value};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};
use wylde_shared::ipc::IpcError;

use crate::config::Config;
use crate::parser::{self, Grammar};

/// The bundled queries for one grammar: `(highlights, locals)`. Highlights
/// may be a concatenation (TS/TSX layering); locals is empty where the
/// grammar ships none.
fn query_sources(grammar: &Grammar) -> (String, &'static str) {
    match grammar.name {
        "python" => (tree_sitter_python::HIGHLIGHTS_QUERY.to_string(), ""),
        "rust" => (tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(), ""),
        "javascript" => (
            format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
            ),
            tree_sitter_javascript::LOCALS_QUERY,
        ),
        "typescript" => (
            format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        "tsx" => (
            format!(
                "{}\n{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        "markdown" => (tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.to_string(), ""),
        // Slice K — config + shell grammars, each with its bundled query.
        "json" => (tree_sitter_json::HIGHLIGHTS_QUERY.to_string(), ""),
        "toml" => (tree_sitter_toml_ng::HIGHLIGHTS_QUERY.to_string(), ""),
        "yaml" => (tree_sitter_yaml::HIGHLIGHTS_QUERY.to_string(), ""),
        "bash" => (tree_sitter_bash::HIGHLIGHT_QUERY.to_string(), ""),
        // A grammar row without a case here is a registry/dispatch bug, not
        // caller input — surface it as unsupported so it's observable.
        _ => (String::new(), ""),
    }
}

/// Resolve the grammar — same contract as `outline`: explicit-unknown is a
/// caller error; an unclaimed extension is `unsupported_language`.
fn resolve_grammar(path: &str, language: Option<&str>) -> Result<&'static Grammar, IpcError> {
    if let Some(lang) = language.filter(|l| !l.trim().is_empty()) {
        return parser::resolve(lang).ok_or_else(|| {
            let known: Vec<&str> = parser::REGISTRY.iter().map(|g| g.name).collect();
            IpcError::new(
                "unknown_language",
                format!("language {lang:?} not linked in this build; known: {known:?}"),
            )
        });
    }
    parser::resolve_by_path(path).ok_or_else(|| {
        IpcError::new(
            "unsupported_language",
            format!("no linked grammar claims the extension of {path:?}"),
        )
    })
}

/// `treesitter.highlight` core. See the module docs for the shape.
///
/// When `inline_source` is `Some`, that text is highlighted directly and the
/// disk read + size check are skipped — `path` is then used only for grammar
/// resolution (its extension). This is the live-editor path: the code editor
/// (IDE S4) highlights its in-memory buffer, which may differ from the
/// on-disk file. When `None`, the file at `path` is read from disk (the
/// original Slice H behaviour, unchanged).
pub fn highlight(
    path: &str,
    language: Option<&str>,
    inline_source: Option<&str>,
) -> Result<Value, IpcError> {
    let cfg = Config::get();

    let source = if let Some(src) = inline_source {
        if src.len() > cfg.max_source_bytes {
            return Err(IpcError::new(
                "invalid_request",
                format!(
                    "inline source is {} bytes; exceeds max_source_bytes={}",
                    src.len(),
                    cfg.max_source_bytes
                ),
            ));
        }
        src.to_owned()
    } else {
        match std::fs::metadata(path) {
            Ok(m) if (m.len() as usize) > cfg.max_source_bytes => {
                return Err(IpcError::new(
                    "invalid_request",
                    format!(
                        "file {path:?} is {} bytes; exceeds max_source_bytes={}",
                        m.len(),
                        cfg.max_source_bytes
                    ),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(IpcError::new(
                    "not_found",
                    format!("could not stat {path:?}: {e}"),
                ));
            }
        }
        std::fs::read_to_string(path)
            .map_err(|e| IpcError::new("read_failed", format!("could not read {path:?}: {e}")))?
    };

    let grammar = resolve_grammar(path, language)?;
    let (highlights, locals) = query_sources(grammar);
    if highlights.is_empty() {
        return Err(IpcError::new(
            "unsupported_language",
            format!("the {} grammar has no highlight query yet", grammar.name),
        ));
    }

    let mut config = HighlightConfiguration::new(
        (grammar.language)(),
        grammar.name,
        &highlights,
        "", // no injections in v1 (module docs)
        locals,
    )
    .map_err(|e| {
        IpcError::new(
            "query_invalid",
            format!(
                "highlight query for {} failed to compile: {e}",
                grammar.name
            ),
        )
    })?;
    // Recognise every capture the bundled query defines — scopes come out
    // exactly as the grammar's maintainers named them.
    let names: Vec<String> = config
        .query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    config.configure(&names);

    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&config, source.as_bytes(), None, |_| None)
        .map_err(|e| IpcError::new("highlight_failed", format!("highlighting failed: {e}")))?;

    // Flatten the event stream into spans: a Source event inside one or more
    // active highlights emits one span scoped to the innermost highlight.
    let mut spans: Vec<Value> = Vec::new();
    let mut active: Vec<usize> = Vec::new();
    for event in events {
        match event
            .map_err(|e| IpcError::new("highlight_failed", format!("highlighting failed: {e}")))?
        {
            HighlightEvent::HighlightStart(h) => active.push(h.0),
            HighlightEvent::HighlightEnd => {
                active.pop();
            }
            HighlightEvent::Source { start, end } => {
                if start == end {
                    continue;
                }
                if let Some(&idx) = active.last() {
                    spans.push(json!({
                        "start_byte": start,
                        "end_byte": end,
                        "scope": names[idx],
                    }));
                }
            }
        }
    }

    Ok(json!({
        "path": path,
        "language": grammar.name,
        "span_count": spans.len(),
        "spans": spans,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_source(src: &str, ext: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(src.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn inline_source_highlights_without_reading_disk() {
        // The live-editor path (IDE S4): grammar resolved from the path's
        // extension, but the bytes come from `inline_source` — the file on
        // disk here is empty, proving disk is never read.
        let f = temp_source("", "rs");
        let out = highlight(
            f.path().to_str().unwrap(),
            None,
            Some("fn main() { let x = 1; }"),
        )
        .unwrap();
        let spans = out["spans"].as_array().unwrap();
        assert!(!spans.is_empty(), "inline source should produce spans");
        assert!(spans
            .iter()
            .any(|s| s["scope"].as_str() == Some("keyword")));
    }

    /// The scope names highlighting `src` produced, plus the raw reply.
    fn scopes_of(src: &str, ext: &str) -> (Vec<String>, Value) {
        let f = temp_source(src, ext);
        let out = highlight(f.path().to_str().unwrap(), None, None).unwrap();
        let scopes = out["spans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["scope"].as_str().unwrap().to_string())
            .collect();
        (scopes, out)
    }

    /// Spans must be in-bounds, non-empty, and non-decreasing — the shared
    /// shape invariant for every grammar.
    fn assert_span_shape(out: &Value, src_len: usize) {
        let spans = out["spans"].as_array().unwrap();
        assert_eq!(out["span_count"], spans.len());
        let mut last_start = 0usize;
        for s in spans {
            let a = s["start_byte"].as_u64().unwrap() as usize;
            let b = s["end_byte"].as_u64().unwrap() as usize;
            assert!(a < b, "empty span");
            assert!(b <= src_len, "span past EOF");
            assert!(a >= last_start, "spans went backwards");
            last_start = a;
            assert!(!s["scope"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn python_keywords_functions_and_strings_highlight() {
        let src = "def greet(name):\n    return \"hi \" + name\n";
        let (scopes, out) = scopes_of(src, "py");
        assert!(
            scopes.iter().any(|s| s.starts_with("keyword")),
            "{scopes:?}"
        );
        assert!(
            scopes.iter().any(|s| s.starts_with("function")),
            "{scopes:?}"
        );
        assert!(scopes.iter().any(|s| s.starts_with("string")), "{scopes:?}");
        assert_span_shape(&out, src.len());
        assert_eq!(out["language"], "python");
    }

    #[test]
    fn rust_highlights_with_the_bundled_query() {
        let src = "fn main() {\n    let msg = \"hello\";\n    println!(\"{msg}\");\n}\n";
        let (scopes, out) = scopes_of(src, "rs");
        assert!(
            scopes.iter().any(|s| s.starts_with("keyword")),
            "{scopes:?}"
        );
        assert!(scopes.iter().any(|s| s.starts_with("string")), "{scopes:?}");
        assert_span_shape(&out, src.len());
    }

    #[test]
    fn typescript_layers_the_js_query() {
        let src = "export function go(n: number): string {\n  return `v${n}`;\n}\n";
        let (scopes, out) = scopes_of(src, "ts");
        assert!(
            scopes.iter().any(|s| s.starts_with("keyword")),
            "{scopes:?}"
        );
        // Type annotations only highlight via the TS layer — proves the
        // concatenation took.
        assert!(scopes.iter().any(|s| s.starts_with("type")), "{scopes:?}");
        assert_span_shape(&out, src.len());
    }

    #[test]
    fn tsx_highlights_jsx_tags() {
        let src = "export function App() {\n  return <div className=\"x\">hi</div>;\n}\n";
        let (scopes, out) = scopes_of(src, "tsx");
        assert!(scopes.iter().any(|s| s.starts_with("tag")), "{scopes:?}");
        assert_span_shape(&out, src.len());
        assert_eq!(out["language"], "tsx");
    }

    #[test]
    fn javascript_highlights_functions() {
        let src = "function boot() {\n  return 42;\n}\n";
        let (scopes, out) = scopes_of(src, "js");
        assert!(
            scopes.iter().any(|s| s.starts_with("keyword")),
            "{scopes:?}"
        );
        assert!(
            scopes.iter().any(|s| s.starts_with("function")),
            "{scopes:?}"
        );
        assert_span_shape(&out, src.len());
    }

    #[test]
    fn markdown_highlights_heading_structure() {
        let src = "# Title\n\nSome text.\n\n```\ncode\n```\n";
        let (scopes, out) = scopes_of(src, "md");
        assert!(!scopes.is_empty(), "markdown produced no spans");
        assert_span_shape(&out, src.len());
        assert_eq!(out["language"], "markdown");
    }

    #[test]
    fn slice_k_grammars_highlight_with_their_bundled_queries() {
        for (src, ext) in [
            ("{\"key\": \"value\", \"n\": 42}\n", "json"),
            ("[server]\nport = 8080\nname = \"x\"\n", "toml"),
            ("top:\n  nested: value\n", "yaml"),
            ("greet() {\n  echo \"hi\"\n}\n", "sh"),
        ] {
            let (scopes, out) = scopes_of(src, ext);
            assert!(!scopes.is_empty(), "{ext} produced no spans");
            assert_span_shape(&out, src.len());
        }
    }

    #[test]
    fn unknown_extension_vs_explicit_unknown_language() {
        let f = temp_source("plain\n", "txt");
        let err = highlight(f.path().to_str().unwrap(), None, None).unwrap_err();
        assert_eq!(err.code, "unsupported_language");
        let err = highlight(f.path().to_str().unwrap(), Some("haskell"), None).unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn missing_file_is_not_found() {
        let err = highlight("C:/no/such/file.rs", None, None).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn empty_file_yields_no_spans() {
        let f = temp_source("", "py");
        let out = highlight(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["span_count"], 0);
    }
}
