//! AST-boundary-aware chunking — the `treesitter.chunk` verb.
//!
//! Splits a source file into chunks that fall on function/class boundaries
//! instead of arbitrary line windows, so a RAG ingest never embeds half a
//! function. The shape mirrors the plan
//! (`docs/plans/treesitter-sidecar.md` §"API surface"):
//!
//! Input  `{path, language?, max_chunk_bytes?}`
//! Output `{chunks:[{start_line, end_line, byte_start, byte_end, kind, symbol_name?}]}`
//!
//! Strategy:
//!   * Read the file (`path` is read by the sidecar; callers pass paths, not
//!     bytes — the same contract `parse` documents for the inline escape hatch).
//!   * Resolve the grammar from `language` or, when omitted, the file
//!     extension. A grammar with a `chunk_query` → AST-aligned chunking; a
//!     known grammar with no query, or an *unknown* language, → byte-window
//!     fallback so we still return *something* useful.
//!   * Run the grammar's chunk query to find top-level definition boundaries,
//!     walk the module's top-level children, and group the leftover statements
//!     (imports, module-level assignments) into "module" filler chunks so the
//!     output covers the whole file contiguously.
//!   * Any single chunk larger than `max_chunk_bytes` is sub-split into
//!     line-aligned byte windows so one giant definition can't produce one
//!     embedding-busting chunk.
//!
//! Chunks return *ranges only* (line + byte offsets), never source bytes, so
//! the reply stays KB-sized even for a large file — the same payload
//! discipline `parse` follows to stay under the 64 MB pipe frame cap.
//!
//! Coordinates: `start_line`/`end_line` are **1-based, inclusive** (editor
//! convention). `byte_start`/`byte_end` are **0-based, half-open** (`byte_end`
//! is exclusive), so `source[byte_start..byte_end]` is exactly the chunk text.

use std::collections::HashSet;

use serde_json::{json, Value};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};
use wylde_shared::ipc::IpcError;

use crate::config::Config;
use crate::parser::{self, Grammar};

/// One emitted chunk. Serialised with `start_line`/`end_line` (1-based,
/// inclusive) + `byte_start`/`byte_end` (0-based, half-open) + a `kind` tag
/// and optional `symbol_name`.
#[derive(Debug, Clone)]
struct Chunk {
    start_line: usize,
    end_line: usize,
    byte_start: usize,
    byte_end: usize,
    /// Node kind for an AST chunk (`function_definition`, `class_definition`,
    /// `decorated_definition`), `"module"` for leftover top-level statements,
    /// or `"window"` for a byte-window fallback chunk.
    kind: String,
    symbol_name: Option<String>,
}

impl Chunk {
    fn to_json(&self) -> Value {
        let mut obj = json!({
            "start_line": self.start_line,
            "end_line": self.end_line,
            "byte_start": self.byte_start,
            "byte_end": self.byte_end,
            "kind": self.kind,
        });
        if let Some(ref name) = self.symbol_name {
            obj["symbol_name"] = json!(name);
        }
        obj
    }
}

/// Resolve which grammar to use: explicit `language` wins; otherwise infer
/// from the path extension. `Ok(None)` means "no linked grammar applies" — a
/// valid state that drives the byte-window fallback (an unknown language is
/// chunked, not rejected).
fn resolve_grammar(
    path: &str,
    language: Option<&str>,
) -> Result<Option<&'static Grammar>, IpcError> {
    match language {
        Some(lang) if !lang.trim().is_empty() => {
            // An explicit-but-unlinked language is a caller error (they named a
            // grammar this build doesn't carry) — distinct from "infer failed".
            parser::resolve(lang).map(Some).ok_or_else(|| {
                let known: Vec<&str> = parser::REGISTRY.iter().map(|g| g.name).collect();
                IpcError::new(
                    "unknown_language",
                    format!("language {lang:?} not linked in this build; known: {known:?}"),
                )
            })
        }
        // No language given → infer from extension; None is fine (→ windows).
        _ => Ok(parser::resolve_by_path(path)),
    }
}

/// `treesitter.chunk` core. See the module docs for the request/response shape.
pub fn chunk(
    path: &str,
    language: Option<&str>,
    max_chunk_bytes: Option<usize>,
) -> Result<Value, IpcError> {
    let cfg = Config::get();
    // A 0 from the caller means "no override" → fall back to the config
    // default; clamp to >=1 so the windowing loop can't stall.
    let max_bytes = max_chunk_bytes
        .filter(|n| *n > 0)
        .unwrap_or(cfg.max_chunk_bytes)
        .max(1);

    // Size-gate before reading the whole file into memory (plan risk #4):
    // a multi-MB minified file shouldn't be slurped just to be rejected.
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

    let source = std::fs::read_to_string(path)
        .map_err(|e| IpcError::new("read_failed", format!("could not read {path:?}: {e}")))?;

    let grammar = resolve_grammar(path, language)?;
    let chunks = match grammar.and_then(|g| g.chunk_query.map(|q| (g, q))) {
        // AST-aligned path: a linked grammar with a chunk query.
        Some((g, query_src)) => ast_chunks(&source, g, query_src, max_bytes)?,
        // Fallback path: unknown language or no query → byte windows.
        None => window_chunks(&source, 0, max_bytes),
    };

    let language_name = grammar.map(|g| g.name);
    Ok(json!({
        "path": path,
        "language": language_name,
        "ast_aware": grammar.is_some_and(|g| g.chunk_query.is_some()),
        "chunk_count": chunks.len(),
        "chunks": chunks.iter().map(Chunk::to_json).collect::<Vec<_>>(),
    }))
}

/// AST-aligned chunking. Runs `query_src` to find top-level boundary nodes,
/// then walks the root's top-level children: a boundary node becomes its own
/// chunk; runs of non-boundary statements coalesce into `"module"` filler
/// chunks. Each result chunk is windowed if it exceeds `max_bytes`.
fn ast_chunks(
    source: &str,
    grammar: &Grammar,
    query_src: &str,
    max_bytes: usize,
) -> Result<Vec<Chunk>, IpcError> {
    let lang = (grammar.language)();

    let mut parser = Parser::new();
    parser.set_language(&lang).map_err(|e| {
        IpcError::new(
            "grammar_load_failed",
            format!("could not load {} grammar: {e}", grammar.name),
        )
    })?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| IpcError::new("parse_failed", "tree-sitter returned no tree"))?;
    let root = tree.root_node();

    let query = Query::new(&lang, query_src).map_err(|e| {
        IpcError::new(
            "query_invalid",
            format!("chunk query for {} failed to compile: {e}", grammar.name),
        )
    })?;
    let chunk_idx = query.capture_index_for_name("chunk");
    let name_idx = query.capture_index_for_name("symbol_name");

    // Boundary node id → its symbol name (if the query captured one). The id
    // lets us recognise a top-level child as a boundary during the walk
    // without re-matching.
    let mut boundary_names: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    let mut boundary_ids: HashSet<usize> = HashSet::new();

    let src_bytes = source.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src_bytes);
    while let Some(m) = matches.next() {
        let mut chunk_node = None;
        let mut sym = None;
        for cap in m.captures {
            if Some(cap.index) == chunk_idx {
                chunk_node = Some(cap.node);
            } else if Some(cap.index) == name_idx {
                sym = cap.node.utf8_text(src_bytes).ok().map(str::to_string);
            }
        }
        if let Some(node) = chunk_node {
            boundary_ids.insert(node.id());
            if let Some(name) = sym {
                boundary_names.insert(node.id(), name);
            }
        }
    }

    // Walk the root's named top-level children in source order and reduce them
    // to *segment starts*: each boundary node starts its own segment; a run of
    // non-boundary statements (imports, module-level assignments) coalesces
    // into one `"module"` filler segment. We only track each segment's content
    // start byte — the spans are tiled below.
    let mut segs: Vec<(usize, &str, Option<String>)> = Vec::new(); // (content_start, kind, symbol)
    let mut in_filler = false;
    let mut walk = root.walk();
    for child in root.named_children(&mut walk) {
        if boundary_ids.contains(&child.id()) {
            in_filler = false;
            segs.push((
                child.start_byte(),
                child.kind(),
                boundary_names.get(&child.id()).cloned(),
            ));
        } else if !in_filler {
            in_filler = true;
            segs.push((child.start_byte(), "module", None));
        }
        // Subsequent filler children just extend the current filler segment —
        // nothing to record, since the span runs to the next segment's start.
    }

    // A file with only comments / whitespace has no named children → emit the
    // whole thing as one window so callers still get a chunk back.
    if segs.is_empty() {
        return Ok(if source.is_empty() {
            Vec::new()
        } else {
            window_chunks(source, 0, max_bytes)
        });
    }

    // Tile the segments so chunks cover the file contiguously — each chunk runs
    // from its own content start to the next segment's start (the first from
    // byte 0, the last to EOF). This absorbs inter-node whitespace/blank lines
    // into the adjacent chunk so no source byte is dropped from the index.
    let mut out: Vec<Chunk> = Vec::new();
    for i in 0..segs.len() {
        let byte_start = if i == 0 { 0 } else { segs[i].0 };
        let byte_end = if i + 1 < segs.len() {
            segs[i + 1].0
        } else {
            source.len()
        };
        let start_line = line_at(source, byte_start);
        let end_line = line_at(source, byte_end.saturating_sub(1));
        push_windowed(
            &mut out,
            source,
            byte_start,
            byte_end,
            start_line,
            end_line,
            segs[i].1,
            segs[i].2.clone(),
            max_bytes,
        );
    }
    Ok(out)
}

/// 1-based line number of the line containing byte offset `byte` (i.e. one
/// more than the count of newlines that precede it).
fn line_at(source: &str, byte: usize) -> usize {
    let upto = byte.min(source.len());
    1 + source.as_bytes()[..upto]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// Push a chunk, sub-splitting into line-aligned byte windows if it exceeds
/// `max_bytes`. Sub-chunks inherit `kind`/`symbol_name`.
#[allow(clippy::too_many_arguments)]
fn push_windowed(
    out: &mut Vec<Chunk>,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    start_line: usize,
    end_line: usize,
    kind: &str,
    symbol_name: Option<String>,
    max_bytes: usize,
) {
    if byte_end.saturating_sub(byte_start) <= max_bytes {
        out.push(Chunk {
            start_line,
            end_line,
            byte_start,
            byte_end,
            kind: kind.to_string(),
            symbol_name,
        });
        return;
    }
    // Oversized: split at line boundaries closest to each window edge.
    let mut sub = window_chunks_range(source, byte_start, byte_end, start_line, max_bytes);
    for (i, c) in sub.iter_mut().enumerate() {
        c.kind = kind.to_string();
        // Keep the symbol on every shard but tag the part so chunks stay
        // distinguishable downstream.
        c.symbol_name = symbol_name.as_ref().map(|n| format!("{n}#part{i}"));
    }
    out.append(&mut sub);
}

/// Byte-window chunking over the whole source from `base_byte` — the
/// unknown-language fallback. Windows break at line boundaries so a chunk
/// never splits a line mid-token.
fn window_chunks(source: &str, base_byte: usize, max_bytes: usize) -> Vec<Chunk> {
    if source.is_empty() {
        return Vec::new();
    }
    window_chunks_range(source, base_byte, source.len(), 1, max_bytes)
}

/// Window `source[from..to]` into line-aligned byte windows of at most
/// `max_bytes`. `first_line` is the 1-based line number of `from`. Chunk kind
/// is `"window"`; callers override it for oversized-AST shards.
fn window_chunks_range(
    source: &str,
    from: usize,
    to: usize,
    first_line: usize,
    max_bytes: usize,
) -> Vec<Chunk> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut win_start = from;
    let mut line = first_line;
    // Line at the current scan position; advanced as we pass '\n'.
    let mut cur_line = first_line;
    let mut i = from;
    while i < to {
        let at_limit = i - win_start >= max_bytes;
        let is_break = bytes[i] == b'\n';
        if at_limit && is_break {
            // Close the window *including* this newline.
            out.push(Chunk {
                start_line: line,
                end_line: cur_line,
                byte_start: win_start,
                byte_end: i + 1,
                kind: "window".to_string(),
                symbol_name: None,
            });
            win_start = i + 1;
            line = cur_line + 1;
        }
        if is_break {
            cur_line += 1;
        }
        i += 1;
    }
    if win_start < to {
        out.push(Chunk {
            start_line: line,
            end_line: cur_line,
            byte_start: win_start,
            byte_end: to,
            kind: "window".to_string(),
            symbol_name: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `src` to a temp file with `ext` and return its path. Kept around
    /// for the test's lifetime via the returned `NamedTempFile`.
    fn temp_source(src: &str, ext: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(src.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn chunks_of(v: &Value) -> &Vec<Value> {
        v["chunks"].as_array().unwrap()
    }

    #[test]
    fn chunks_python_on_def_and_class_boundaries() {
        let src = "import os\n\
                   \n\
                   def alpha():\n    return 1\n\
                   \n\
                   class Beta:\n    def m(self):\n        return 2\n\
                   \n\
                   x = alpha()\n";
        let f = temp_source(src, "py");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["language"], "python");
        assert_eq!(out["ast_aware"], true);

        let cs = chunks_of(&out);
        // Expect: module(import) | function alpha | class Beta | module(x=...)
        let kinds: Vec<&str> = cs.iter().map(|c| c["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"function_definition"));
        assert!(kinds.contains(&"class_definition"));
        assert!(kinds.contains(&"module"));

        // The class chunk must keep its method INSIDE it (one chunk, not split).
        let beta = cs
            .iter()
            .find(|c| c["symbol_name"] == json!("Beta"))
            .expect("class chunk present");
        assert_eq!(beta["kind"], "class_definition");
        // Method `m` is nested, so it is NOT a separate top-level chunk.
        assert!(cs.iter().all(|c| c["symbol_name"] != json!("m")));

        let alpha = cs
            .iter()
            .find(|c| c["symbol_name"] == json!("alpha"))
            .expect("function chunk present");
        assert_eq!(alpha["kind"], "function_definition");
    }

    #[test]
    fn chunks_are_contiguous_and_cover_the_file() {
        let src = "import os\n\ndef a():\n    return 1\n\ndef b():\n    return 2\n";
        let f = temp_source(src, "py");
        let out = chunk(f.path().to_str().unwrap(), Some("python"), None).unwrap();
        let cs = chunks_of(&out);
        assert!(!cs.is_empty());
        // Byte ranges tile the file with no gaps or overlaps.
        let mut cursor = 0usize;
        for c in cs {
            assert_eq!(c["byte_start"].as_u64().unwrap() as usize, cursor);
            cursor = c["byte_end"].as_u64().unwrap() as usize;
        }
        assert_eq!(cursor, src.len());
    }

    #[test]
    fn decorated_definition_is_one_named_chunk() {
        let src = "@decorator\ndef wrapped():\n    return 1\n";
        let f = temp_source(src, "py");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        let cs = chunks_of(&out);
        let dec = cs
            .iter()
            .find(|c| c["kind"] == json!("decorated_definition"))
            .expect("decorated chunk present");
        // Reaches through to the inner function name.
        assert_eq!(dec["symbol_name"], json!("wrapped"));
        // The decorator line is part of this chunk (starts at line 1).
        assert_eq!(dec["start_line"], json!(1));
    }

    #[test]
    fn oversized_definition_is_windowed() {
        // One function whose body dwarfs a tiny max_chunk_bytes.
        let body: String = (0..200).map(|i| format!("    x{i} = {i}\n")).collect();
        let src = format!("def big():\n{body}");
        let f = temp_source(&src, "py");
        let out = chunk(f.path().to_str().unwrap(), Some("python"), Some(64)).unwrap();
        let cs = chunks_of(&out);
        // The single def split into multiple shards, each <= a line over 64B.
        assert!(cs.len() > 1, "expected the giant def to be windowed");
        assert!(cs.iter().all(|c| c["kind"] == json!("function_definition")));
        // Shards are part-tagged.
        assert_eq!(cs[0]["symbol_name"], json!("big#part0"));
    }

    #[test]
    fn unknown_language_falls_back_to_windows() {
        let src = "alpha beta gamma\ndelta epsilon\nzeta\n";
        let f = temp_source(src, "txt");
        let out = chunk(f.path().to_str().unwrap(), None, Some(8)).unwrap();
        assert_eq!(out["ast_aware"], false);
        assert_eq!(out["language"], Value::Null);
        let cs = chunks_of(&out);
        assert!(cs.iter().all(|c| c["kind"] == json!("window")));
        // Windows tile the whole file.
        let total: usize = cs
            .iter()
            .map(|c| {
                c["byte_end"].as_u64().unwrap() as usize
                    - c["byte_start"].as_u64().unwrap() as usize
            })
            .sum();
        assert_eq!(total, src.len());
    }

    #[test]
    fn explicit_unknown_language_is_rejected() {
        // An explicit-but-unlinked language is a caller error (vs. an
        // unknown *extension*, which falls back to byte windows).
        let f = temp_source("main = putStrLn \"hi\"\n", "hs");
        let err = chunk(f.path().to_str().unwrap(), Some("haskell"), None).unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn missing_file_is_not_found() {
        let err = chunk("C:/no/such/file/here.py", Some("python"), None).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn empty_file_yields_no_chunks() {
        let f = temp_source("", "py");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(chunks_of(&out).len(), 0);
    }

    /// Every chunk's byte range must tile the file contiguously (shared
    /// invariant across all AST grammars — no source byte dropped from the index).
    fn assert_contiguous(out: &Value, src: &str) {
        let cs = chunks_of(out);
        assert!(!cs.is_empty());
        let mut cursor = 0usize;
        for c in cs {
            assert_eq!(c["byte_start"].as_u64().unwrap() as usize, cursor);
            cursor = c["byte_end"].as_u64().unwrap() as usize;
        }
        assert_eq!(cursor, src.len());
    }

    #[test]
    fn chunks_rust_on_item_boundaries() {
        let src = "use std::fmt;\n\
                   \nfn main() {\n    run();\n}\n\
                   \nstruct Cfg {\n    n: u32,\n}\n\
                   \nimpl Cfg {\n    fn run(&self) {}\n}\n";
        let f = temp_source(src, "rs");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["language"], "rust");
        assert_eq!(out["ast_aware"], true);
        let cs = chunks_of(&out);
        let kinds: Vec<&str> = cs.iter().map(|c| c["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"function_item"));
        assert!(kinds.contains(&"struct_item"));
        assert!(kinds.contains(&"impl_item"));
        // The `fn main` chunk is named; its method `run` stays inside the impl.
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("main")));
        assert!(cs.iter().all(|c| c["symbol_name"] != json!("run")));
        assert_contiguous(&out, src);
    }

    #[test]
    fn chunks_typescript_on_class_and_interface() {
        let src = "import { x } from './x';\n\
                   \nexport function go(): void {}\n\
                   \ninterface Opts {\n  n: number;\n}\n\
                   \nclass Widget {\n  render() {}\n}\n";
        let f = temp_source(src, "ts");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["language"], "typescript");
        assert_eq!(out["ast_aware"], true);
        let cs = chunks_of(&out);
        // `export function go` is wrapped in an export_statement boundary.
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("go")));
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("Opts")));
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("Widget")));
        assert_contiguous(&out, src);
    }

    #[test]
    fn chunks_javascript_on_function_and_class() {
        let src = "const a = 1;\n\
                   \nfunction boot() {\n  start();\n}\n\
                   \nclass View {\n  render() {}\n}\n";
        let f = temp_source(src, "js");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["language"], "javascript");
        let cs = chunks_of(&out);
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("boot")));
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("View")));
        assert_contiguous(&out, src);
    }

    #[test]
    fn chunks_tsx_on_component_boundaries_with_jsx_inside() {
        // A `.tsx` file with a function component (whose body holds JSX) and a
        // class component. JSX must stay *inside* its component chunk, not split
        // it — the whole point of the dedicated TSX grammar.
        let src = "import { Child } from './child';\n\
                   \nexport function App(): JSX.Element {\n\
                   \n  return <div><Child /></div>;\n}\n\
                   \nclass Panel extends Component {\n  render() {\n    return <Child />;\n  }\n}\n";
        let f = temp_source(src, "tsx");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["language"], "tsx");
        assert_eq!(out["ast_aware"], true);
        let cs = chunks_of(&out);
        // `export function App` (wrapped in export_statement) and `class Panel`
        // are each their own named chunk.
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("App")));
        assert!(cs.iter().any(|c| c["symbol_name"] == json!("Panel")));
        // The method `render` stays inside the Panel chunk, not a top-level one.
        assert!(cs.iter().all(|c| c["symbol_name"] != json!("render")));
        assert_contiguous(&out, src);
    }

    #[test]
    fn chunks_markdown_on_sections_with_heading_names() {
        let src =
            "# Intro\n\nFirst paragraph.\n\n# Usage\n\nSecond paragraph.\n\n## Detail\n\nNested.\n";
        let f = temp_source(src, "md");
        let out = chunk(f.path().to_str().unwrap(), None, None).unwrap();
        assert_eq!(out["language"], "markdown");
        assert_eq!(out["ast_aware"], true);
        let cs = chunks_of(&out);
        // Two top-level (H1) sections; the H2 rides inside its parent section.
        let names: Vec<String> = cs
            .iter()
            .filter_map(|c| c["symbol_name"].as_str().map(|s| s.trim().to_string()))
            .collect();
        assert!(names.iter().any(|n| n == "Intro"), "names: {names:?}");
        assert!(names.iter().any(|n| n == "Usage"), "names: {names:?}");
        assert_contiguous(&out, src);
    }
}
