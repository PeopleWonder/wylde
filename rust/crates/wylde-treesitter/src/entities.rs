//! Structural entity extraction — the `treesitter.extract_entities` verb.
//!
//! Walks a parsed file and emits the structural entities the graph layer
//! turns into nodes + typed edges (`docs/plans/treesitter-sidecar.md`
//! §"API surface", Slice 3):
//!
//! Input  `{path, language?}`
//! Output `{path, language, module,
//!          functions:[{name,line}],
//!          classes:[{name,line,methods:[…],bases:[…]}],
//!          imports:[{module,line}],
//!          calls:[{caller,callee,line}],
//!          counts:{…}}`
//!
//! The shape is a **superset** of the plan's documented surface — it adds
//! `bases` (so the Memgraph writer can emit `INHERITS` edges), a `module`
//! identity (the import edge source + the caller for module-level calls), and
//! a `counts` summary. Every added field is additive, so a consumer that only
//! reads `functions/classes/imports/calls` is unaffected.
//!
//! Why this maps cleanly onto Memgraph (no protocol work — see the plan):
//!   * `memgraph.upsert(chunks=[{… entities:[name,…]}])` wants a **flat list of
//!     entity-name strings** per chunk. [`entity_names`] flattens this output
//!     into exactly that.
//!   * `memgraph.relate(rel_type, pairs=[{source,target}])` validates
//!     `rel_type ∈ {CALLS, IMPORTS, INHERITS, …}`. `calls` → `CALLS`
//!     (`caller→callee`), `imports` → `IMPORTS` (`module→imported`),
//!     `classes[].bases` → `INHERITS` (`class→base`). No new routes.
//!
//! Coordinates: every `line` is **1-based** (editor convention) — the line the
//! definition / import / call starts on.
//!
//! Resolution scope (plan risk #6): tree-sitter gives *syntactic* names, not
//! resolved targets. A `CALLS` edge is name-matched (`foo()` → callee `"foo"`),
//! an attribute call `obj.method()` yields callee `"method"`, and a base class
//! is recorded by its written name. Good enough for graph expansion; not a
//! type-resolved call graph.

use std::path::Path;

use serde_json::{json, Value};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};
use wylde_shared::ipc::IpcError;

use crate::config::Config;
use crate::parser::{self, Grammar};

/// `treesitter.extract_entities` core. See the module docs for the
/// request/response shape.
pub fn extract_entities(path: &str, language: Option<&str>) -> Result<Value, IpcError> {
    let cfg = Config::get();

    // Size-gate before slurping the file (plan risk #4) — a multi-MB minified
    // file shouldn't be read just to balloon the parser's node count.
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

    let source = std::fs::read_to_string(path).map_err(|e| {
        IpcError::new("read_failed", format!("could not read {path:?}: {e}"))
    })?;

    // Entities need an AST: unlike `chunk` (which byte-windows an unknown
    // language) there's nothing meaningful to extract without a grammar, so an
    // unknown language is a hard error — same contract as `parse`.
    let grammar = resolve_grammar(path, language)?;
    let query_src = grammar.entity_query.ok_or_else(|| {
        IpcError::new(
            "unsupported_language",
            format!("{} has no entity query in this build", grammar.name),
        )
    })?;

    let module = module_name(path);
    let entities = walk(&source, grammar, query_src, &module)?;

    Ok(json!({
        "path": path,
        "language": grammar.name,
        "module": module,
        "functions": entities.functions.iter().map(NamedLine::to_json).collect::<Vec<_>>(),
        "classes": entities.classes.iter().map(ClassInfo::to_json).collect::<Vec<_>>(),
        "imports": entities.imports.iter().map(|i| json!({"module": i.name, "line": i.line})).collect::<Vec<_>>(),
        "calls": entities.calls.iter().map(Call::to_json).collect::<Vec<_>>(),
        "counts": {
            "functions": entities.functions.len(),
            "classes": entities.classes.len(),
            "imports": entities.imports.len(),
            "calls": entities.calls.len(),
        },
    }))
}

/// Flatten an `extract_entities` reply into the **flat `[name, …]` list** that
/// `memgraph.upsert(chunks=[{… entities}])` expects: every function, class,
/// method, base, imported module, and call endpoint, plus the file's own
/// module identity — deduplicated, source order preserved. The Memgraph writer
/// (N8N) can attach these to a chunk so the Entity nodes that the `CALLS` /
/// `IMPORTS` / `INHERITS` edges connect actually exist.
pub fn entity_names(reply: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |s: Option<&str>| {
        if let Some(s) = s {
            if !s.is_empty() && seen.insert(s.to_string()) {
                out.push(s.to_string());
            }
        }
    };
    push(reply.get("module").and_then(Value::as_str));
    if let Some(fs) = reply.get("functions").and_then(Value::as_array) {
        for f in fs {
            push(f.get("name").and_then(Value::as_str));
        }
    }
    if let Some(cs) = reply.get("classes").and_then(Value::as_array) {
        for c in cs {
            push(c.get("name").and_then(Value::as_str));
            for m in c.get("methods").and_then(Value::as_array).into_iter().flatten() {
                push(m.as_str());
            }
            for b in c.get("bases").and_then(Value::as_array).into_iter().flatten() {
                push(b.as_str());
            }
        }
    }
    for i in reply.get("imports").and_then(Value::as_array).into_iter().flatten() {
        push(i.get("module").and_then(Value::as_str));
    }
    for c in reply.get("calls").and_then(Value::as_array).into_iter().flatten() {
        push(c.get("callee").and_then(Value::as_str));
    }
    out
}

// ── internal model ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct NamedLine {
    name: String,
    line: usize,
}
impl NamedLine {
    fn to_json(&self) -> Value {
        json!({"name": self.name, "line": self.line})
    }
}

#[derive(Debug, Clone)]
struct ClassInfo {
    name: String,
    line: usize,
    methods: Vec<String>,
    bases: Vec<String>,
}
impl ClassInfo {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "line": self.line,
            "methods": self.methods,
            "bases": self.bases,
        })
    }
}

#[derive(Debug, Clone)]
struct Call {
    caller: String,
    callee: String,
    line: usize,
}
impl Call {
    fn to_json(&self) -> Value {
        json!({"caller": self.caller, "callee": self.callee, "line": self.line})
    }
}

#[derive(Default)]
struct Entities {
    functions: Vec<NamedLine>,
    classes: Vec<ClassInfo>,
    imports: Vec<NamedLine>,
    calls: Vec<Call>,
}

/// Resolve which grammar to use: explicit `language` wins; otherwise infer from
/// the path extension. Unlike `chunk`, an unknown language is rejected (no
/// AST → nothing to extract).
fn resolve_grammar(
    path: &str,
    language: Option<&str>,
) -> Result<&'static Grammar, IpcError> {
    match language {
        Some(lang) if !lang.trim().is_empty() => parser::resolve(lang).ok_or_else(|| {
            let known: Vec<&str> = parser::REGISTRY.iter().map(|g| g.name).collect();
            IpcError::new(
                "unknown_language",
                format!("language {lang:?} not linked in this build; known: {known:?}"),
            )
        }),
        _ => parser::resolve_by_path(path).ok_or_else(|| {
            IpcError::new(
                "unknown_language",
                format!("could not infer a linked grammar from path {path:?}; pass `language`"),
            )
        }),
    }
}

/// File stem as the module identity (`a/b/ingest.py` → `ingest`). Used as the
/// `IMPORTS` edge source and the caller for module-level calls so those edges
/// anchor on a stable node. Falls back to the raw path if there's no stem.
fn module_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string())
}

/// Run the entity query, then classify each captured node. Functions/classes
/// are classified by their enclosing definition; calls get their caller from
/// the nearest enclosing function (else the module).
fn walk(
    source: &str,
    grammar: &Grammar,
    query_src: &str,
    module: &str,
) -> Result<Entities, IpcError> {
    let lang = (grammar.language)();
    let mut parser = Parser::new();
    parser.set_language(&lang).map_err(|e| {
        IpcError::new(
            "grammar_load_failed",
            format!("could not load {} grammar: {e}", grammar.name),
        )
    })?;
    let tree = parser.parse(source, None).ok_or_else(|| {
        IpcError::new("parse_failed", "tree-sitter returned no tree")
    })?;
    let root = tree.root_node();

    let query = Query::new(&lang, query_src).map_err(|e| {
        IpcError::new(
            "query_invalid",
            format!("entity query for {} failed to compile: {e}", grammar.name),
        )
    })?;
    let func_idx = query.capture_index_for_name("function");
    let class_idx = query.capture_index_for_name("class");
    let import_idx = query.capture_index_for_name("import");
    let call_idx = query.capture_index_for_name("call");

    let src = source.as_bytes();
    let mut out = Entities::default();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let node = cap.node;
            let idx = Some(cap.index);
            if idx == func_idx {
                // Only *top-level* functions go in `functions`; methods are
                // carried by their class, nested closures are skipped (their
                // calls are still attributed to them as a caller).
                if enclosing_def(node).is_none() {
                    if let Some(name) = field_text(node, "name", src) {
                        out.functions.push(NamedLine { name, line: line_of(node) });
                    }
                }
            } else if idx == class_idx {
                if enclosing_def(node).is_none() {
                    out.classes.push(class_info(node, src));
                }
            } else if idx == import_idx {
                collect_imports(node, src, &mut out.imports);
            } else if idx == call_idx {
                if let Some(callee) = callee_name(node, src) {
                    let caller = enclosing_function_name(node, src)
                        .unwrap_or_else(|| module.to_string());
                    out.calls.push(Call { caller, callee, line: line_of(node) });
                }
            }
        }
    }
    Ok(out)
}

/// 1-based start line of a node.
fn line_of(node: Node) -> usize {
    node.start_position().row + 1
}

/// Text of `node`'s `field` child, if present and valid UTF-8.
fn field_text(node: Node, field: &str, src: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(src).ok())
        .map(str::to_string)
}

/// Nearest ancestor that is a `function_definition` or `class_definition`
/// (skips the node itself and any `decorated_definition` wrapper). `None` means
/// the node is at module top level. Drives top-level-vs-method classification.
fn enclosing_def(node: Node) -> Option<Node> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "function_definition" | "class_definition" => return Some(n),
            _ => cur = n.parent(),
        }
    }
    None
}

/// Name of the nearest enclosing `function_definition` — the caller for a call
/// expression. `None` at module level (the caller falls back to the module).
fn enclosing_function_name(node: Node, src: &[u8]) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "function_definition" {
            return field_text(n, "name", src);
        }
        cur = n.parent();
    }
    None
}

/// Build a [`ClassInfo`] from a `class_definition`: its name, line, the bases
/// in `superclasses`, and the names of the `def`s directly in its body.
fn class_info(node: Node, src: &[u8]) -> ClassInfo {
    let name = field_text(node, "name", src).unwrap_or_default();
    let line = line_of(node);

    // Bases: identifiers/attributes in the `superclasses` argument list. Skip
    // keyword arguments (`metaclass=…`), which aren't inheritance.
    let mut bases = Vec::new();
    if let Some(args) = node.child_by_field_name("superclasses") {
        let mut c = args.walk();
        for arg in args.named_children(&mut c) {
            match arg.kind() {
                "identifier" | "attribute" => {
                    if let Ok(t) = arg.utf8_text(src) {
                        bases.push(t.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    // Methods: `def`s directly in the class body (a `decorated_definition`
    // wraps the real def). Nested classes / nested defs are NOT methods.
    let mut methods = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut c = body.walk();
        for stmt in body.named_children(&mut c) {
            let def = if stmt.kind() == "decorated_definition" {
                stmt.child_by_field_name("definition")
            } else {
                Some(stmt)
            };
            if let Some(d) = def {
                if d.kind() == "function_definition" {
                    if let Some(n) = field_text(d, "name", src) {
                        methods.push(n);
                    }
                }
            }
        }
    }

    ClassInfo { name, line, methods, bases }
}

/// Pull module name(s) from an `import_statement` / `import_from_statement`.
/// `import a, b.c` → two imports; `from x.y import z` → one import of `x.y`.
fn collect_imports(node: Node, src: &[u8], out: &mut Vec<NamedLine>) {
    let line = line_of(node);
    match node.kind() {
        "import_statement" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                let module = match child.kind() {
                    // `import os` / `import os.path`
                    "dotted_name" => child.utf8_text(src).ok().map(str::to_string),
                    // `import os as o` — the real module is the `name` field.
                    "aliased_import" => field_text(child, "name", src),
                    _ => None,
                };
                if let Some(m) = module {
                    out.push(NamedLine { name: m, line });
                }
            }
        }
        "import_from_statement" => {
            // `from <module_name> import …` — record the source module. A
            // relative import (`from . import x`) has its dots in the text.
            if let Some(m) = field_text(node, "module_name", src) {
                out.push(NamedLine { name: m, line });
            }
        }
        _ => {}
    }
}

/// Resolve a `call` node's callee name. `foo()` → `"foo"`; `obj.method()` →
/// `"method"` (the attribute's final identifier, the useful edge target).
/// Returns `None` for calls whose target isn't a plain name (e.g. `f()()`,
/// `arr[0]()`) — those don't yield a meaningful name-level edge.
fn callee_name(call: Node, src: &[u8]) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    match f.kind() {
        "identifier" => f.utf8_text(src).ok().map(str::to_string),
        "attribute" => field_text(f, "attribute", src),
        _ => None,
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

    fn extract(src: &str) -> Value {
        let f = temp_source(src, "py");
        extract_entities(f.path().to_str().unwrap(), None).unwrap()
    }

    #[test]
    fn extracts_top_level_functions() {
        let v = extract("def alpha():\n    return 1\n\ndef beta(x):\n    return x\n");
        let fns = v["functions"].as_array().unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(fns[0]["line"], 1);
        assert_eq!(fns[1]["line"], 4);
    }

    #[test]
    fn class_carries_methods_and_bases_not_top_level_functions() {
        let src = "class Widget(Base, mixins.Loud):\n\
                   \n    def __init__(self):\n        self.x = 1\n\
                   \n    def render(self):\n        return self.x\n";
        let v = extract(src);
        let classes = v["classes"].as_array().unwrap();
        assert_eq!(classes.len(), 1);
        let w = &classes[0];
        assert_eq!(w["name"], "Widget");
        assert_eq!(w["line"], 1);
        let methods: Vec<&str> = w["methods"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
        assert_eq!(methods, vec!["__init__", "render"]);
        let bases: Vec<&str> = w["bases"].as_array().unwrap().iter().map(|b| b.as_str().unwrap()).collect();
        assert_eq!(bases, vec!["Base", "mixins.Loud"]);
        // Methods must NOT leak into top-level functions.
        assert!(v["functions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn extracts_imports_both_forms() {
        let src = "import os\nimport os.path as p\nimport sys, json\nfrom collections import OrderedDict\nfrom . import sibling\n";
        let v = extract(src);
        let imports: Vec<&str> = v["imports"].as_array().unwrap().iter().map(|i| i["module"].as_str().unwrap()).collect();
        assert!(imports.contains(&"os"));
        assert!(imports.contains(&"os.path")); // aliased → real module name
        assert!(imports.contains(&"sys"));
        assert!(imports.contains(&"json"));
        assert!(imports.contains(&"collections"));
    }

    #[test]
    fn extracts_calls_with_caller_scope() {
        let src = "import os\n\ndef worker():\n    helper()\n    os.getcwd()\n\ntop_level_call()\n";
        let v = extract(src);
        let calls = v["calls"].as_array().unwrap();
        // helper() inside worker → caller worker, callee helper.
        let helper = calls.iter().find(|c| c["callee"] == "helper").unwrap();
        assert_eq!(helper["caller"], "worker");
        // os.getcwd() → callee is the attribute's final name.
        let getcwd = calls.iter().find(|c| c["callee"] == "getcwd").unwrap();
        assert_eq!(getcwd["caller"], "worker");
        // A module-level call is attributed to the module identity (file stem).
        let top = calls.iter().find(|c| c["callee"] == "top_level_call").unwrap();
        assert_eq!(top["caller"], v["module"]);
    }

    #[test]
    fn method_calls_are_attributed_to_the_method() {
        let src = "class C:\n    def run(self):\n        self.step()\n        compute()\n";
        let v = extract(src);
        let calls = v["calls"].as_array().unwrap();
        let step = calls.iter().find(|c| c["callee"] == "step").unwrap();
        assert_eq!(step["caller"], "run");
        let compute = calls.iter().find(|c| c["callee"] == "compute").unwrap();
        assert_eq!(compute["caller"], "run");
    }

    #[test]
    fn entity_names_flattens_for_memgraph_upsert() {
        let src = "import os\n\nclass A(Base):\n    def m(self):\n        helper()\n\ndef f():\n    g()\n";
        let v = extract(src);
        let names = entity_names(&v);
        // module identity + class + base + method + functions + callees, deduped.
        assert!(names.contains(&"A".to_string()));
        assert!(names.contains(&"Base".to_string()));
        assert!(names.contains(&"m".to_string()));
        assert!(names.contains(&"f".to_string()));
        assert!(names.contains(&"helper".to_string()));
        assert!(names.contains(&"g".to_string()));
        assert!(names.contains(&"os".to_string()));
        assert!(names.contains(&v["module"].as_str().unwrap().to_string()));
        // Deduplicated.
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn counts_match_arrays() {
        let v = extract("import os\n\ndef a():\n    pass\n\nclass B:\n    pass\n");
        assert_eq!(v["counts"]["functions"], v["functions"].as_array().unwrap().len());
        assert_eq!(v["counts"]["classes"], v["classes"].as_array().unwrap().len());
        assert_eq!(v["counts"]["imports"], v["imports"].as_array().unwrap().len());
    }

    #[test]
    fn unknown_language_is_rejected() {
        let f = temp_source("fn main() {}\n", "rs");
        let err = extract_entities(f.path().to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn explicit_unknown_language_is_rejected() {
        let f = temp_source("x = 1\n", "py");
        let err = extract_entities(f.path().to_str().unwrap(), Some("rust")).unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn missing_file_is_not_found() {
        let err = extract_entities("C:/no/such/file.py", Some("python")).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn empty_file_yields_empty_entities() {
        let v = extract("");
        assert_eq!(v["functions"].as_array().unwrap().len(), 0);
        assert_eq!(v["classes"].as_array().unwrap().len(), 0);
        assert_eq!(v["calls"].as_array().unwrap().len(), 0);
    }
}
