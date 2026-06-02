//! Language registry + the file→tree primitive.
//!
//! Slice 1 links exactly ONE grammar (Python). The registry is a static
//! table so adding a grammar later is one row plus a `Cargo.toml` dep —
//! that expansion is Slice 5 (see the plan). Every verb that takes a
//! `language` resolves it through [`resolve`]; `treesitter.languages`
//! enumerates [`REGISTRY`].

use serde_json::{json, Value};
use tree_sitter::{Language, Node, Parser};
use wylde_shared::ipc::IpcError;

use crate::config::Config;

/// One statically-linked grammar.
pub struct Grammar {
    /// Canonical lowercase language id (`"python"`). Also the value callers
    /// pass as `language` and the key [`resolve`] matches on.
    pub name: &'static str,

    /// Identifier for *which* grammar is linked, so a drift is observable in
    /// the `treesitter.languages` reply (plan risk #2). Slice 1 reports the
    /// pinned `tree-sitter-<lang>` crate version; a true content-hash of the
    /// generated `parser.c` awaits grammar vendoring (Slice 5). Keep in sync
    /// with the `Cargo.toml` pin.
    pub grammar_sha: &'static str,

    /// Builds the tree-sitter `Language`. A fn pointer (not a built
    /// `Language`) keeps [`REGISTRY`] a `const` and defers FFI to call time.
    pub language: fn() -> Language,

    /// File extensions (no dot, lowercase) this grammar owns. Lets a verb
    /// infer `language` from a `path` when the caller omits it.
    pub extensions: &'static [&'static str],

    /// Tree-sitter query (`.scm` source) that captures top-level chunk
    /// boundaries — `@chunk` per boundary node, optional `@symbol_name`. Used
    /// by [`crate::chunk`]. `None` means the grammar has no chunk query yet, so
    /// chunking falls back to byte windows.
    pub chunk_query: Option<&'static str>,
}

/// Every grammar this build links. Slice 1–2: Python only.
pub static REGISTRY: &[Grammar] = &[Grammar {
    name: "python",
    grammar_sha: "tree-sitter-python@0.25",
    language: || tree_sitter_python::LANGUAGE.into(),
    extensions: &["py", "pyi"],
    chunk_query: Some(include_str!("queries/python/chunks.scm")),
}];

/// Infer a grammar from a file path's extension (case-insensitive). `None`
/// when no linked grammar claims the extension.
pub fn resolve_by_path(path: &str) -> Option<&'static Grammar> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    REGISTRY
        .iter()
        .find(|g| g.extensions.iter().any(|x| *x == ext))
}

/// Resolve a language id (case-insensitive) to its grammar. `None` if the
/// build doesn't link it.
pub fn resolve(name: &str) -> Option<&'static Grammar> {
    let lc = name.trim().to_ascii_lowercase();
    REGISTRY.iter().find(|g| g.name == lc)
}

/// The `treesitter.languages` payload: `{languages:[{name, grammar_sha, abi}]}`.
/// `abi` is the tree-sitter parser ABI version the linked grammar was
/// generated against — the number that must stay compatible with the
/// `tree-sitter` runtime.
pub fn languages() -> Value {
    let langs: Vec<Value> = REGISTRY
        .iter()
        .map(|g| {
            let lang = (g.language)();
            json!({
                "name": g.name,
                "grammar_sha": g.grammar_sha,
                "abi": lang.abi_version(),
            })
        })
        .collect();
    json!({ "languages": langs })
}

/// Parse inline `source` in `language` to a bounded AST sketch.
///
/// Slice-1 escape hatch — it proves the grammar loads and parses; it returns
/// node *kinds and ranges*, never source bytes, so the reply stays small.
/// Depth is bounded by [`Config::max_parse_depth`] and input size by
/// [`Config::max_source_bytes`] so a pathological file can't OOM the parser
/// or overflow the pipe frame.
pub fn parse(source: &str, language: &str) -> Result<Value, IpcError> {
    let cfg = Config::get();

    if source.len() > cfg.max_source_bytes {
        return Err(IpcError::new(
            "invalid_request",
            format!(
                "source is {} bytes; exceeds max_source_bytes={}",
                source.len(),
                cfg.max_source_bytes
            ),
        ));
    }

    let grammar = resolve(language).ok_or_else(|| {
        let known: Vec<&str> = REGISTRY.iter().map(|g| g.name).collect();
        IpcError::new(
            "unknown_language",
            format!("language {language:?} not linked in this build; known: {known:?}"),
        )
    })?;

    let mut parser = Parser::new();
    parser.set_language(&(grammar.language)()).map_err(|e| {
        IpcError::new(
            "grammar_load_failed",
            format!("could not load {} grammar: {e}", grammar.name),
        )
    })?;

    let tree = parser.parse(source, None).ok_or_else(|| {
        IpcError::new(
            "parse_failed",
            "tree-sitter returned no tree (parse timed out or was cancelled)",
        )
    })?;

    let root = tree.root_node();
    Ok(json!({
        "language": grammar.name,
        "has_error": root.has_error(),
        "root": node_sketch(&root, 0, cfg.max_parse_depth),
    }))
}

/// Recursively serialise a node to `{kind, named, start_byte, end_byte,
/// start_point, end_point, children}` — ranges only, no source text. At
/// `max_depth` the `children` array is omitted and `truncated:true` is set.
fn node_sketch(node: &Node, depth: usize, max_depth: usize) -> Value {
    let start = node.start_position();
    let end = node.end_position();
    let mut obj = json!({
        "kind": node.kind(),
        "named": node.is_named(),
        "start_byte": node.start_byte(),
        "end_byte": node.end_byte(),
        "start_point": {"row": start.row, "column": start.column},
        "end_point": {"row": end.row, "column": end.column},
    });

    if depth >= max_depth {
        if node.child_count() > 0 {
            obj["truncated"] = json!(true);
            obj["child_count"] = json!(node.child_count());
        }
        return obj;
    }

    let mut cursor = node.walk();
    let children: Vec<Value> = node
        .children(&mut cursor)
        .map(|c| node_sketch(&c, depth + 1, max_depth))
        .collect();
    if !children.is_empty() {
        obj["children"] = json!(children);
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_python_only() {
        assert_eq!(REGISTRY.len(), 1);
        assert_eq!(REGISTRY[0].name, "python");
    }

    #[test]
    fn resolve_is_case_insensitive_and_trims() {
        assert!(resolve("python").is_some());
        assert!(resolve("  PyThOn ").is_some());
        assert!(resolve("rust").is_none());
    }

    #[test]
    fn resolve_by_path_maps_python_extensions() {
        assert_eq!(resolve_by_path("a/b/c.py").unwrap().name, "python");
        assert_eq!(resolve_by_path("MOD.PY").unwrap().name, "python");
        assert_eq!(resolve_by_path("stub.pyi").unwrap().name, "python");
        assert!(resolve_by_path("main.rs").is_none());
        assert!(resolve_by_path("no_extension").is_none());
    }

    #[test]
    fn python_grammar_carries_a_chunk_query() {
        assert!(resolve("python").unwrap().chunk_query.is_some());
    }

    #[test]
    fn languages_reports_python_with_abi() {
        let v = languages();
        let arr = v["languages"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "python");
        assert_eq!(arr[0]["grammar_sha"], "tree-sitter-python@0.25");
        // ABI must be a positive integer the runtime accepted.
        assert!(arr[0]["abi"].as_u64().unwrap() > 0);
    }

    #[test]
    fn parse_python_snippet_produces_module_root() {
        let out = parse("def greet(name):\n    return name\n", "python").unwrap();
        assert_eq!(out["language"], "python");
        assert_eq!(out["has_error"], false);
        assert_eq!(out["root"]["kind"], "module");
        // A function def should be somewhere in the first level of children.
        let kids = out["root"]["children"].as_array().unwrap();
        assert!(kids.iter().any(|k| k["kind"] == "function_definition"));
    }

    #[test]
    fn parse_flags_syntax_error() {
        let out = parse("def (:", "python").unwrap();
        assert_eq!(out["has_error"], true);
    }

    #[test]
    fn parse_unknown_language_errors() {
        let err = parse("fn main(){}", "rust").unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn parse_rejects_oversized_source() {
        // Build a string just over the default 2 MiB ceiling.
        let big = "x".repeat(2 * 1024 * 1024 + 1);
        let err = parse(&big, "python").unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }
}
