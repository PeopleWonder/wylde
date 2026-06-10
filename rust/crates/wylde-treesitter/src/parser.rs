//! Language registry + the file→tree primitive.
//!
//! Slice 1 linked exactly ONE grammar (Python). Slice 4 expands the registry
//! to the plan's recommended v1 set — Python, Rust, TypeScript, JavaScript,
//! Markdown (`docs/plans/treesitter-sidecar.md` §"Dependencies & grammar
//! strategy"). The registry is a static table so adding a grammar is one row
//! plus a `Cargo.toml` dep. Every verb that takes a `language` resolves it
//! through [`resolve`] (or [`resolve_by_path`] from a file extension);
//! `treesitter.languages` enumerates [`REGISTRY`].

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

    /// Tree-sitter query (`.scm` source) that captures structural entities —
    /// `@function`/`@class`/`@import`/`@call` candidate nodes that
    /// [`crate::entities`] classifies. Used by `treesitter.extract_entities`.
    /// `None` means the grammar has no entity query yet, so the verb rejects
    /// with `unsupported_language` (unlike chunking, there's no useful
    /// fallback without an AST).
    pub entity_query: Option<&'static str>,

    /// The per-language node-kind/field metadata [`crate::entities`] consults
    /// to classify the [`Self::entity_query`] captures (a `@function` is named
    /// via this language's name field; a `@class`'s methods/bases are read from
    /// these node kinds, etc.). `Some` exactly when [`Self::entity_query`] is —
    /// the query says *what* to capture, the spec says *how* to read it. A
    /// chunk-only grammar (Markdown) leaves both `None`.
    pub entity_spec: Option<&'static crate::entities::EntitySpec>,

    /// Tree-sitter query (`.scm` source) that captures outline items at every
    /// depth — `@item` per definition node, `@name` for its identifier. Used
    /// by [`crate::outline`] (Slice H), which nests the flat captures into a
    /// tree by byte containment. `None` → `treesitter.outline` rejects with
    /// `unsupported_language` (no useful outline without an AST).
    pub outline_query: Option<&'static str>,
}

/// Every grammar this build links. Slice 4: Python, Rust, TypeScript, TSX,
/// JavaScript, Markdown. Markdown is chunk-only (no code entities to extract —
/// `entity_query`/`entity_spec` `None`); the rest carry both.
///
/// TypeScript and TSX are two grammars from the SAME `tree-sitter-typescript`
/// crate: `LANGUAGE_TYPESCRIPT` (the `.ts` grammar) and `LANGUAGE_TSX` (which
/// also parses JSX). They're separate rows because `.tsx`/JSX silently
/// misparses under the non-TSX parser. `.jsx` rides the JavaScript grammar,
/// which parses JSX natively — only `.tsx` (TypeScript + JSX) needs the
/// dedicated TSX grammar.
pub static REGISTRY: &[Grammar] = &[
    Grammar {
        name: "python",
        grammar_sha: "tree-sitter-python@0.25",
        language: || tree_sitter_python::LANGUAGE.into(),
        extensions: &["py", "pyi"],
        chunk_query: Some(include_str!("queries/python/chunks.scm")),
        entity_query: Some(include_str!("queries/python/entities.scm")),
        entity_spec: Some(&crate::entities::PYTHON_SPEC),
        outline_query: Some(include_str!("queries/python/outline.scm")),
    },
    Grammar {
        name: "rust",
        grammar_sha: "tree-sitter-rust@0.24",
        language: || tree_sitter_rust::LANGUAGE.into(),
        extensions: &["rs"],
        chunk_query: Some(include_str!("queries/rust/chunks.scm")),
        entity_query: Some(include_str!("queries/rust/entities.scm")),
        entity_spec: Some(&crate::entities::RUST_SPEC),
        outline_query: Some(include_str!("queries/rust/outline.scm")),
    },
    Grammar {
        name: "typescript",
        grammar_sha: "tree-sitter-typescript@0.23",
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        extensions: &["ts", "mts", "cts"],
        chunk_query: Some(include_str!("queries/typescript/chunks.scm")),
        entity_query: Some(include_str!("queries/typescript/entities.scm")),
        entity_spec: Some(&crate::entities::TS_SPEC),
        outline_query: Some(include_str!("queries/typescript/outline.scm")),
    },
    Grammar {
        name: "tsx",
        // Same crate/pin as TypeScript — a second exported grammar, not a new
        // dependency. The grammar_sha matches `typescript` deliberately.
        grammar_sha: "tree-sitter-typescript@0.23",
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        extensions: &["tsx"],
        chunk_query: Some(include_str!("queries/tsx/chunks.scm")),
        entity_query: Some(include_str!("queries/tsx/entities.scm")),
        // TSX node kinds/fields are identical to TS (it's TS + JSX), so the TS
        // spec reads its entities verbatim; the TSX `.scm` adds the JSX captures.
        entity_spec: Some(&crate::entities::TS_SPEC),
        outline_query: Some(include_str!("queries/tsx/outline.scm")),
    },
    Grammar {
        name: "javascript",
        grammar_sha: "tree-sitter-javascript@0.23",
        language: || tree_sitter_javascript::LANGUAGE.into(),
        // The JS grammar parses JSX too, so `.jsx` rides here.
        extensions: &["js", "jsx", "mjs", "cjs"],
        chunk_query: Some(include_str!("queries/javascript/chunks.scm")),
        entity_query: Some(include_str!("queries/javascript/entities.scm")),
        entity_spec: Some(&crate::entities::JS_SPEC),
        outline_query: Some(include_str!("queries/javascript/outline.scm")),
    },
    Grammar {
        name: "markdown",
        grammar_sha: "tree-sitter-md@0.3",
        // The block grammar (`LANGUAGE`); the inline grammar is unused —
        // section/heading structure is all the chunker needs.
        language: || tree_sitter_md::LANGUAGE.into(),
        extensions: &["md", "markdown"],
        chunk_query: Some(include_str!("queries/markdown/chunks.scm")),
        // Markdown has no functions/classes/imports/calls — chunk-only for
        // entities, but it DOES outline (the heading hierarchy).
        entity_query: None,
        entity_spec: None,
        outline_query: Some(include_str!("queries/markdown/outline.scm")),
    },
];

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
    fn registry_lists_the_slice4_grammars() {
        let names: Vec<&str> = REGISTRY.iter().map(|g| g.name).collect();
        assert_eq!(
            names,
            vec![
                "python",
                "rust",
                "typescript",
                "tsx",
                "javascript",
                "markdown"
            ]
        );
    }

    #[test]
    fn resolve_is_case_insensitive_and_trims() {
        assert!(resolve("python").is_some());
        assert!(resolve("  PyThOn ").is_some());
        assert!(resolve("rust").is_some());
        assert!(resolve("TypeScript").is_some());
        assert!(resolve("TSX").is_some());
        assert!(resolve("haskell").is_none());
    }

    #[test]
    fn resolve_by_path_maps_extensions_to_grammars() {
        assert_eq!(resolve_by_path("a/b/c.py").unwrap().name, "python");
        assert_eq!(resolve_by_path("MOD.PY").unwrap().name, "python");
        assert_eq!(resolve_by_path("stub.pyi").unwrap().name, "python");
        assert_eq!(resolve_by_path("src/main.rs").unwrap().name, "rust");
        assert_eq!(resolve_by_path("app.ts").unwrap().name, "typescript");
        assert_eq!(resolve_by_path("util.mts").unwrap().name, "typescript");
        assert_eq!(resolve_by_path("index.js").unwrap().name, "javascript");
        // `.jsx` rides the JS grammar (it parses JSX natively).
        assert_eq!(resolve_by_path("View.jsx").unwrap().name, "javascript");
        assert_eq!(resolve_by_path("README.md").unwrap().name, "markdown");
        // `.tsx` resolves to the dedicated TSX grammar (TypeScript + JSX).
        assert_eq!(resolve_by_path("Component.tsx").unwrap().name, "tsx");
        assert_eq!(resolve_by_path("App.TSX").unwrap().name, "tsx");
        assert!(resolve_by_path("no_extension").is_none());
    }

    #[test]
    fn code_grammars_carry_chunk_and_entity_queries_in_lockstep_with_specs() {
        for g in REGISTRY {
            // Every linked grammar chunks.
            assert!(g.chunk_query.is_some(), "{} has no chunk query", g.name);
            // entity_query and entity_spec are present together or not at all.
            assert_eq!(
                g.entity_query.is_some(),
                g.entity_spec.is_some(),
                "{} query/spec mismatch",
                g.name
            );
        }
        // Markdown is the chunk-only grammar.
        assert!(resolve("markdown").unwrap().entity_query.is_none());
        assert!(resolve("rust").unwrap().entity_query.is_some());
    }

    #[test]
    fn languages_reports_every_grammar_with_abi() {
        let v = languages();
        let arr = v["languages"].as_array().unwrap();
        assert_eq!(arr.len(), 6);
        assert_eq!(arr[0]["name"], "python");
        assert_eq!(arr[0]["grammar_sha"], "tree-sitter-python@0.25");
        // Every grammar reports a positive ABI the runtime accepted.
        for g in arr {
            assert!(g["abi"].as_u64().unwrap() > 0, "{} has no abi", g["name"]);
        }
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
    fn parse_links_rust_now() {
        let out = parse("fn main() { let x = 1; }\n", "rust").unwrap();
        assert_eq!(out["language"], "rust");
        assert_eq!(out["has_error"], false);
        assert_eq!(out["root"]["kind"], "source_file");
    }

    #[test]
    fn parse_links_tsx_with_jsx() {
        // JSX inside a TS function — this is exactly what `LANGUAGE_TYPESCRIPT`
        // would misparse and the dedicated TSX grammar parses cleanly.
        let out = parse(
            "function App(): JSX.Element {\n  return <div className=\"x\"><Child /></div>;\n}\n",
            "tsx",
        )
        .unwrap();
        assert_eq!(out["language"], "tsx");
        assert_eq!(out["has_error"], false);
        assert_eq!(out["root"]["kind"], "program");
    }

    #[test]
    fn parse_unknown_language_errors() {
        let err = parse("module Main where", "haskell").unwrap_err();
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
