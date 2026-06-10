//! Per-file symbol outline — the `treesitter.outline` verb (Slice H, plan
//! §"API surface" / slice 4 "IDE verbs").
//!
//! Input  `{path, language?}`
//! Output `{tree:[{kind, name, line, end_line, children:[…]}]}`
//!
//! Strategy: run the grammar's `outline_query` (captures `@item` definition
//! nodes at EVERY depth + `@name` identifiers), then nest the flat matches
//! into a tree by byte containment — a method whose range lies inside a
//! class's range becomes that class's child. Containment nesting is
//! grammar-agnostic, so one Rust pass serves all six languages; the per-
//! language knowledge lives entirely in the `.scm` files.
//!
//! Ranges only, never source bytes (the standard payload discipline), except
//! `name` — identifier-sized text. `line`/`end_line` are 1-based inclusive
//! (editor convention, same as `chunk`).

use serde_json::{json, Value};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};
use wylde_shared::ipc::IpcError;

use crate::config::Config;
use crate::parser::{self, Grammar};

/// One flat outline item before nesting.
#[derive(Debug)]
struct Item {
    byte_start: usize,
    byte_end: usize,
    kind: String,
    name: Option<String>,
    line: usize,
    end_line: usize,
    children: Vec<Item>,
}

impl Item {
    fn to_json(&self) -> Value {
        let mut obj = json!({
            "kind": self.kind,
            "name": self.name,
            "line": self.line,
            "end_line": self.end_line,
        });
        if !self.children.is_empty() {
            obj["children"] = json!(self.children.iter().map(Item::to_json).collect::<Vec<_>>());
        }
        obj
    }
}

/// Resolve the grammar: explicit `language` wins (unknown → caller error);
/// otherwise infer from the extension. Unlike `chunk` there is no useful
/// fallback without an AST, so an unresolvable file is `unsupported_language`
/// (the `extract_entities` precedent).
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

/// `treesitter.outline` core. See the module docs for the shape.
pub fn outline(path: &str, language: Option<&str>) -> Result<Value, IpcError> {
    let cfg = Config::get();

    // Size-gate before slurping (plan risk #4) — chunk.rs discipline.
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
    let query_src = grammar.outline_query.ok_or_else(|| {
        IpcError::new(
            "unsupported_language",
            format!("the {} grammar has no outline query yet", grammar.name),
        )
    })?;

    let tree = build_tree(&source, grammar, query_src)?;
    Ok(json!({
        "path": path,
        "language": grammar.name,
        "item_count": count(&tree),
        "tree": tree.iter().map(Item::to_json).collect::<Vec<_>>(),
    }))
}

fn count(items: &[Item]) -> usize {
    items.len() + items.iter().map(|i| count(&i.children)).sum::<usize>()
}

/// Parse + query + nest. Pure over `source` (no IO) so tests can drive it
/// through `outline` with temp files only at the edge.
fn build_tree(source: &str, grammar: &Grammar, query_src: &str) -> Result<Vec<Item>, IpcError> {
    let lang = (grammar.language)();
    let mut ts_parser = Parser::new();
    ts_parser.set_language(&lang).map_err(|e| {
        IpcError::new(
            "grammar_load_failed",
            format!("could not load {} grammar: {e}", grammar.name),
        )
    })?;
    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| IpcError::new("parse_failed", "tree-sitter returned no tree"))?;

    let query = Query::new(&lang, query_src).map_err(|e| {
        IpcError::new(
            "query_invalid",
            format!("outline query for {} failed to compile: {e}", grammar.name),
        )
    })?;
    let item_idx = query.capture_index_for_name("item");
    let name_idx = query.capture_index_for_name("name");

    // Collect flat items, deduping multi-pattern captures of the same node
    // (Rust's two `impl_item` patterns) by node id — the named match wins.
    let src_bytes = source.as_bytes();
    let mut flat: Vec<(usize, Item)> = Vec::new(); // (node_id, item)
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), src_bytes);
    while let Some(m) = matches.next() {
        let mut node = None;
        let mut name = None;
        for cap in m.captures {
            if Some(cap.index) == item_idx {
                node = Some(cap.node);
            } else if Some(cap.index) == name_idx {
                name = cap
                    .node
                    .utf8_text(src_bytes)
                    .ok()
                    .map(|s| s.trim().to_string());
            }
        }
        let Some(node) = node else { continue };
        if let Some((_, existing)) = flat.iter_mut().find(|(id, _)| *id == node.id()) {
            // Same node matched again (impl two-pattern dedup): keep the name
            // whichever pattern captured it.
            if existing.name.is_none() {
                existing.name = name;
            }
            continue;
        }
        flat.push((
            node.id(),
            Item {
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
                kind: node.kind().to_string(),
                name,
                line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                children: Vec::new(),
            },
        ));
    }

    // Nest by byte containment. Sorted by (start asc, end desc) a parent
    // always precedes its children, so a simple stack tiles the tree.
    let mut items: Vec<Item> = flat.into_iter().map(|(_, i)| i).collect();
    items.sort_by(|a, b| {
        a.byte_start
            .cmp(&b.byte_start)
            .then(b.byte_end.cmp(&a.byte_end))
    });

    let mut roots: Vec<Item> = Vec::new();
    let mut stack: Vec<Item> = Vec::new();
    for item in items {
        // Pop completed ancestors (anything that doesn't contain this item).
        while let Some(top) = stack.last() {
            if item.byte_start >= top.byte_end {
                let done = stack.pop().expect("non-empty");
                attach(&mut roots, &mut stack, done);
            } else {
                break;
            }
        }
        stack.push(item);
    }
    while let Some(done) = stack.pop() {
        attach(&mut roots, &mut stack, done);
    }
    Ok(roots)
}

/// Attach a completed item to its parent (the new stack top) or the roots.
fn attach(roots: &mut Vec<Item>, stack: &mut [Item], done: Item) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(done),
        None => roots.push(done),
    }
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

    fn names_at(level: &Value) -> Vec<String> {
        level
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["name"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn python_methods_nest_under_their_class() {
        let src = "def top():\n    return 1\n\nclass Widget:\n    def render(self):\n        pass\n    def hide(self):\n        pass\n";
        let f = temp_source(src, "py");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(out["language"], "python");
        assert_eq!(out["item_count"], 4);
        let roots = names_at(&out["tree"]);
        assert_eq!(roots, vec!["top", "Widget"]);
        let widget = &out["tree"][1];
        assert_eq!(widget["kind"], "class_definition");
        assert_eq!(widget["line"], 4);
        assert_eq!(names_at(&widget["children"]), vec!["render", "hide"]);
    }

    #[test]
    fn rust_impl_methods_nest_and_the_impl_is_named() {
        let src = "struct Cfg {\n    n: u32,\n}\n\nimpl Cfg {\n    fn load() -> Self {\n        todo!()\n    }\n    fn save(&self) {}\n}\n\nfn main() {}\n";
        let f = temp_source(src, "rs");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        let roots: Vec<&str> = out["tree"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["kind"].as_str().unwrap())
            .collect();
        assert_eq!(roots, vec!["struct_item", "impl_item", "function_item"]);
        // The two-pattern impl dedups to ONE item, named after the type.
        let imp = &out["tree"][1];
        assert_eq!(imp["name"], "Cfg");
        assert_eq!(names_at(&imp["children"]), vec!["load", "save"]);
    }

    #[test]
    fn typescript_interface_and_class_members_outline() {
        let src = "interface Opts {\n  load(): void;\n}\n\nexport class Widget {\n  render() {}\n}\n\ntype Alias = string;\n";
        let f = temp_source(src, "ts");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        let tree = out["tree"].as_array().unwrap();
        let opts = tree.iter().find(|i| i["name"] == "Opts").expect("Opts");
        assert_eq!(names_at(&opts["children"]), vec!["load"]);
        let widget = tree.iter().find(|i| i["name"] == "Widget").expect("Widget");
        assert_eq!(names_at(&widget["children"]), vec!["render"]);
        assert!(tree.iter().any(|i| i["name"] == "Alias"));
    }

    #[test]
    fn tsx_component_outlines_as_its_function() {
        let src = "export function App(): JSX.Element {\n  return <div><Child /></div>;\n}\n";
        let f = temp_source(src, "tsx");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(out["language"], "tsx");
        assert_eq!(names_at(&out["tree"]), vec!["App"]);
    }

    #[test]
    fn javascript_class_methods_nest() {
        let src = "function boot() {}\n\nclass View {\n  render() {}\n}\n";
        let f = temp_source(src, "js");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(names_at(&out["tree"]), vec!["boot", "View"]);
        assert_eq!(names_at(&out["tree"][1]["children"]), vec!["render"]);
    }

    #[test]
    fn markdown_outlines_the_heading_hierarchy() {
        let src = "# Intro\n\nText.\n\n## Setup\n\nMore.\n\n## Usage\n\n# Appendix\n";
        let f = temp_source(src, "md");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(out["language"], "markdown");
        let roots = names_at(&out["tree"]);
        assert_eq!(roots, vec!["Intro", "Appendix"]);
        assert_eq!(
            names_at(&out["tree"][0]["children"]),
            vec!["Setup", "Usage"]
        );
    }

    #[test]
    fn slice_k_grammars_outline_their_structure() {
        // JSON: keys nest by object containment.
        let f = temp_source("{\"server\": {\"port\": 1}, \"name\": \"x\"}\n", "json");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(names_at(&out["tree"]), vec!["server", "name"]);
        assert_eq!(names_at(&out["tree"][0]["children"]), vec!["port"]);

        // TOML: tables + array-of-table elements, header-named.
        let f = temp_source("[server]\nport = 1\n\n[[bin]]\nname = \"a\"\n", "toml");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(names_at(&out["tree"]), vec!["server", "bin"]);

        // YAML: mapping keys nest.
        let f = temp_source("top:\n  inner: 1\nother: 2\n", "yaml");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(names_at(&out["tree"]), vec!["top", "other"]);
        assert_eq!(names_at(&out["tree"][0]["children"]), vec!["inner"]);

        // Bash: function definitions.
        let f = temp_source("greet() {\n  echo hi\n}\n", "sh");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(names_at(&out["tree"]), vec!["greet"]);
    }

    #[test]
    fn nested_defs_nest_arbitrarily_deep() {
        let src = "class Outer:\n    class Inner:\n        def deep(self):\n            pass\n";
        let f = temp_source(src, "py");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        let outer = &out["tree"][0];
        let inner = &outer["children"][0];
        assert_eq!(inner["name"], "Inner");
        assert_eq!(names_at(&inner["children"]), vec!["deep"]);
    }

    #[test]
    fn empty_file_outlines_to_an_empty_tree() {
        let f = temp_source("", "py");
        let out = outline(f.path().to_str().unwrap(), None).unwrap();
        assert_eq!(out["item_count"], 0);
        assert_eq!(out["tree"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn unknown_extension_is_unsupported_explicit_unknown_is_caller_error() {
        let f = temp_source("plain text\n", "txt");
        let err = outline(f.path().to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.code, "unsupported_language");
        let err = outline(f.path().to_str().unwrap(), Some("haskell")).unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn missing_file_is_not_found() {
        let err = outline("C:/no/such/file.py", None).unwrap_err();
        assert_eq!(err.code, "not_found");
    }
}
