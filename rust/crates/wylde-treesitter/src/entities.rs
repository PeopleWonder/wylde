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

// ── per-language extraction spec ─────────────────────────────────────────────
//
// The `.scm` entity query is language-*agnostic* in its capture names
// (`@function`/`@class`/`@import`/`@call`) — it says *what* to capture. This
// spec says *how to read* each capture for one language: which field names a
// definition, which node kinds open a scope, where a class body / its methods /
// its bases live, and how that language spells imports and inheritance. One
// spec per grammar keeps every node-kind string in a single table instead of
// scattered through the walk, so adding a grammar is "write a `.scm` + a spec".

/// How a `@class` capture's base/parent types are read — the source of
/// `INHERITS` edges. Dispatched on the class node's *kind* where a language has
/// more than one class-like shape (Rust `impl` vs `trait`).
#[derive(Clone, Copy)]
pub enum BasesStrategy {
    /// No inheritance concept (e.g. a Rust `struct`/`enum`).
    None,
    /// Python: identifiers/attributes in the `superclasses` argument list.
    PythonSuperclasses,
    /// JS/TS: type names inside a `class_heritage` / `extends*` / `implements`
    /// clause.
    JsHeritage,
    /// Rust: `impl Trait for Type` → `Trait`; `trait T: Super` → `Super`.
    Rust,
}

/// How an `@import` capture's module name(s) are read — the source of `IMPORTS`
/// edges.
#[derive(Clone, Copy)]
pub enum ImportStrategy {
    /// Python `import a, b.c` / `from x.y import z`.
    Python,
    /// Rust `use a::b::c;` / `use a::b::{c, d};` → the module path `a::b`.
    RustUse,
    /// ES `import … from "mod"` / `import "mod"` → the `source` string.
    EsModule,
}

/// Per-language node-kind/field metadata for [`walk`]. See the module-level
/// note above for the division of labour with the `.scm` query.
pub struct EntitySpec {
    /// Field that holds a definition's identifier (`name` everywhere we link).
    pub name_field: &'static str,
    /// Definition node kinds that establish an *enclosing scope* — a
    /// `@function`/`@class` whose ancestor chain hits one of these is nested
    /// (a method / inner def), not top-level.
    pub scope_kinds: &'static [&'static str],
    /// Function node kinds whose name is the *caller* for a call nested inside
    /// them (methods included, so a call in a method is attributed to it).
    pub function_kinds: &'static [&'static str],
    /// Field holding a class/impl/trait/interface body.
    pub body_field: &'static str,
    /// Fields tried in order to name a `@class` node — Rust `impl` has no
    /// `name`, so it falls through to `type`.
    pub class_name_fields: &'static [&'static str],
    /// Node kinds counted as methods inside a class body.
    pub method_kinds: &'static [&'static str],
    /// Body-child wrapper kinds to unwrap (via a `definition` field) to reach a
    /// method — Python's `decorated_definition`. Empty for languages without
    /// such a wrapper.
    pub method_wrapper_kinds: &'static [&'static str],
    pub bases: BasesStrategy,
    pub imports: ImportStrategy,
}

pub static PYTHON_SPEC: EntitySpec = EntitySpec {
    name_field: "name",
    scope_kinds: &["function_definition", "class_definition"],
    function_kinds: &["function_definition"],
    body_field: "body",
    class_name_fields: &["name"],
    method_kinds: &["function_definition"],
    method_wrapper_kinds: &["decorated_definition"],
    bases: BasesStrategy::PythonSuperclasses,
    imports: ImportStrategy::Python,
};

pub static RUST_SPEC: EntitySpec = EntitySpec {
    name_field: "name",
    // `impl`/`trait` hold methods; a `fn` nests inner fns. struct/enum carry no
    // fns so they needn't gate scope.
    scope_kinds: &["function_item", "impl_item", "trait_item"],
    function_kinds: &["function_item"],
    body_field: "body",
    // struct/enum/trait name via `name`; `impl` has only `type`.
    class_name_fields: &["name", "type"],
    method_kinds: &["function_item", "function_signature_item"],
    method_wrapper_kinds: &[],
    bases: BasesStrategy::Rust,
    imports: ImportStrategy::RustUse,
};

pub static JS_SPEC: EntitySpec = EntitySpec {
    name_field: "name",
    scope_kinds: &[
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
        "class_declaration",
        "arrow_function",
        "function_expression",
    ],
    function_kinds: &[
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
    ],
    body_field: "body",
    class_name_fields: &["name"],
    method_kinds: &["method_definition"],
    method_wrapper_kinds: &[],
    bases: BasesStrategy::JsHeritage,
    imports: ImportStrategy::EsModule,
};

pub static TS_SPEC: EntitySpec = EntitySpec {
    name_field: "name",
    scope_kinds: &[
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
        "method_signature",
        "abstract_method_signature",
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "arrow_function",
        "function_expression",
    ],
    function_kinds: &[
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
    ],
    body_field: "body",
    class_name_fields: &["name"],
    method_kinds: &[
        "method_definition",
        "method_signature",
        "abstract_method_signature",
    ],
    method_wrapper_kinds: &[],
    bases: BasesStrategy::JsHeritage,
    imports: ImportStrategy::EsModule,
};

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

    let source = std::fs::read_to_string(path)
        .map_err(|e| IpcError::new("read_failed", format!("could not read {path:?}: {e}")))?;

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
    // `entity_query` and `entity_spec` are wired in lockstep in the registry, so
    // a query without a spec is a build bug, not a caller error.
    let spec = grammar.entity_spec.ok_or_else(|| {
        IpcError::new(
            "unsupported_language",
            format!("{} has an entity query but no spec", grammar.name),
        )
    })?;

    let module = module_name(path);
    let entities = walk(&source, grammar, query_src, spec, &module)?;

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
            for m in c
                .get("methods")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                push(m.as_str());
            }
            for b in c
                .get("bases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                push(b.as_str());
            }
        }
    }
    for i in reply
        .get("imports")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        push(i.get("module").and_then(Value::as_str));
    }
    for c in reply
        .get("calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
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
fn resolve_grammar(path: &str, language: Option<&str>) -> Result<&'static Grammar, IpcError> {
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

/// Run the entity query, then classify each captured node per the language
/// [`EntitySpec`]. Functions/classes are classified by their enclosing
/// definition; calls get their caller from the nearest enclosing function
/// (else the module). The `@`-capture *meaning* is uniform across languages;
/// the spec supplies the node kinds/fields used to read each one.
fn walk(
    source: &str,
    grammar: &Grammar,
    query_src: &str,
    spec: &EntitySpec,
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
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| IpcError::new("parse_failed", "tree-sitter returned no tree"))?;
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
    // TSX-only: present when the grammar's `.scm` captures JSX tag names. `None`
    // for every other grammar, so the JSX branch below never fires for them.
    let jsx_call_idx = query.capture_index_for_name("jsx_call");

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
                if enclosing_def(node, spec).is_none() {
                    if let Some(name) = field_text(node, spec.name_field, src) {
                        out.functions.push(NamedLine {
                            name,
                            line: line_of(node),
                        });
                    }
                }
            } else if idx == class_idx {
                if enclosing_def(node, spec).is_none() {
                    out.classes.push(class_info(node, src, spec));
                }
            } else if idx == import_idx {
                collect_imports(node, src, spec, &mut out.imports);
            } else if idx == call_idx {
                if let Some(callee) = callee_name(node, src) {
                    let caller = enclosing_function_name(node, src, spec)
                        .unwrap_or_else(|| module.to_string());
                    out.calls.push(Call {
                        caller,
                        callee,
                        line: line_of(node),
                    });
                }
            } else if idx == jsx_call_idx {
                // A JSX tag name node (`<Foo/>` / `<Foo>…`). React convention:
                // a Capitalized tag is a component reference (→ a CALLS edge);
                // a lowercase tag is a host element (`<div>`), not a call.
                if let Some(callee) = jsx_component_name(node, src) {
                    let caller = enclosing_function_name(node, src, spec)
                        .unwrap_or_else(|| module.to_string());
                    out.calls.push(Call {
                        caller,
                        callee,
                        line: line_of(node),
                    });
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

/// Nearest ancestor whose kind is one of `spec.scope_kinds` (a function/class/
/// method/impl). `None` means the node is at module top level. Drives
/// top-level-vs-nested classification.
fn enclosing_def<'a>(node: Node<'a>, spec: &EntitySpec) -> Option<Node<'a>> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if spec.scope_kinds.contains(&n.kind()) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// Name of the nearest enclosing function (kind in `spec.function_kinds`) — the
/// caller for a call expression. `None` at module level (the caller falls back
/// to the module identity).
fn enclosing_function_name(node: Node, src: &[u8], spec: &EntitySpec) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if spec.function_kinds.contains(&n.kind()) {
            return field_text(n, spec.name_field, src);
        }
        cur = n.parent();
    }
    None
}

/// The leading identifier of a (possibly generic / qualified) *type* reference:
/// `Foo` → `Foo`, `Foo<T>` → `Foo`, `a.B`/`a::B` → the first identifier. The
/// base/parent type for an `INHERITS` edge. Pre-order, first identifier wins.
fn head_type_name(node: Node, src: &[u8]) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier" | "type_identifier" | "field_identifier" | "property_identifier"
    ) {
        return node.utf8_text(src).ok().map(str::to_string);
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if let Some(n) = head_type_name(child, src) {
            return Some(n);
        }
    }
    None
}

/// The *trailing* identifier of a (possibly qualified / member) reference:
/// `foo` → `foo`, `obj.method`/`a::b::read` → `method`/`read`. The useful
/// name-level edge target for a callee. Pre-order, last identifier wins.
fn final_identifier(node: Node, src: &[u8]) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
    ) {
        return node.utf8_text(src).ok().map(str::to_string);
    }
    // Recurse into the last named child so a qualified path yields its tail.
    let mut last = None;
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        last = Some(child);
    }
    last.and_then(|n| final_identifier(n, src))
}

/// Build a [`ClassInfo`] from a `@class` node: its name (first present of
/// `spec.class_name_fields`), line, bases (per `spec.bases`), and the methods
/// directly in its body (`spec.method_kinds`, unwrapping `method_wrapper_kinds`).
fn class_info(node: Node, src: &[u8], spec: &EntitySpec) -> ClassInfo {
    let name = class_name(node, src, spec);
    let line = line_of(node);
    let bases = extract_bases(node, src, spec);

    let mut methods = Vec::new();
    if let Some(body) = node.child_by_field_name(spec.body_field) {
        let mut c = body.walk();
        for stmt in body.named_children(&mut c) {
            // Unwrap a wrapper (Python `decorated_definition`) to its real def.
            let def = if spec.method_wrapper_kinds.contains(&stmt.kind()) {
                stmt.child_by_field_name("definition")
            } else {
                Some(stmt)
            };
            if let Some(d) = def {
                if spec.method_kinds.contains(&d.kind()) {
                    if let Some(n) = field_text(d, spec.name_field, src) {
                        methods.push(n);
                    }
                }
            }
        }
    }

    ClassInfo {
        name,
        line,
        methods,
        bases,
    }
}

/// Name a `@class` node: try each of `spec.class_name_fields` in order (Rust
/// `impl` has no `name`, only `type`), reading the field's head type name.
fn class_name(node: Node, src: &[u8], spec: &EntitySpec) -> String {
    for field in spec.class_name_fields {
        if let Some(fc) = node.child_by_field_name(field) {
            if let Some(n) = head_type_name(fc, src) {
                return n;
            }
            if let Ok(t) = fc.utf8_text(src) {
                return t.to_string();
            }
        }
    }
    String::new()
}

/// Extract a `@class` node's base/parent types per the language strategy.
fn extract_bases(node: Node, src: &[u8], spec: &EntitySpec) -> Vec<String> {
    match spec.bases {
        BasesStrategy::None => Vec::new(),
        BasesStrategy::PythonSuperclasses => {
            // identifiers/attributes in `superclasses`; skip kwargs (`metaclass=`).
            let mut bases = Vec::new();
            if let Some(args) = node.child_by_field_name("superclasses") {
                let mut c = args.walk();
                for arg in args.named_children(&mut c) {
                    if matches!(arg.kind(), "identifier" | "attribute") {
                        if let Ok(t) = arg.utf8_text(src) {
                            bases.push(t.to_string());
                        }
                    }
                }
            }
            bases
        }
        BasesStrategy::JsHeritage => {
            // `extends`/`implements` types live in a `class_heritage` (classes)
            // or a direct `extends_type_clause` (interfaces).
            let mut bases = Vec::new();
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                match child.kind() {
                    "class_heritage" => {
                        let mut h = child.walk();
                        for gc in child.named_children(&mut h) {
                            collect_heritage_types(gc, src, &mut bases);
                        }
                    }
                    "extends_clause" | "implements_clause" | "extends_type_clause" => {
                        collect_heritage_types(child, src, &mut bases);
                    }
                    _ => {}
                }
            }
            bases
        }
        BasesStrategy::Rust => {
            let mut bases = Vec::new();
            match node.kind() {
                // `impl Trait for Type` → the implemented trait is the base.
                "impl_item" => {
                    if let Some(t) = node.child_by_field_name("trait") {
                        if let Some(n) = head_type_name(t, src) {
                            bases.push(n);
                        }
                    }
                }
                // `trait T: Super + Other` → supertrait bounds.
                "trait_item" => {
                    if let Some(b) = node.child_by_field_name("bounds") {
                        let mut c = b.walk();
                        for tb in b.named_children(&mut c) {
                            if let Some(n) = head_type_name(tb, src) {
                                bases.push(n);
                            }
                        }
                    }
                }
                _ => {}
            }
            bases
        }
    }
}

/// Collect the leading type name of each clause entry into `out`. A heritage
/// clause (`extends A, B`) holds one entry per parent; an entry may itself be a
/// clause node (JS `class_heritage` wraps `extends_clause`) — recurse one level.
fn collect_heritage_types(node: Node, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "extends_clause" | "implements_clause" | "extends_type_clause" => {
            let mut c = node.walk();
            for entry in node.named_children(&mut c) {
                if let Some(n) = head_type_name(entry, src) {
                    out.push(n);
                }
            }
        }
        _ => {
            if let Some(n) = head_type_name(node, src) {
                out.push(n);
            }
        }
    }
}

/// Pull module name(s) from an `@import` node per the language strategy.
fn collect_imports(node: Node, src: &[u8], spec: &EntitySpec, out: &mut Vec<NamedLine>) {
    let line = line_of(node);
    match spec.imports {
        ImportStrategy::Python => match node.kind() {
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
                // `from <module_name> import …` — a relative import
                // (`from . import x`) carries its dots in the text.
                if let Some(m) = field_text(node, "module_name", src) {
                    out.push(NamedLine { name: m, line });
                }
            }
            _ => {}
        },
        ImportStrategy::RustUse => {
            // `use a::b::c;` / `use a::b::{c, d};` → the module path `a::b`. A
            // scoped path's `path` field is the prefix; a bare `use serde;`
            // argument has no `path`, so use its whole text.
            if let Some(arg) = node.child_by_field_name("argument") {
                let module = arg
                    .child_by_field_name("path")
                    .or(Some(arg))
                    .and_then(|n| n.utf8_text(src).ok())
                    .map(str::to_string);
                if let Some(m) = module {
                    out.push(NamedLine { name: m, line });
                }
            }
        }
        ImportStrategy::EsModule => {
            // `import … from "mod"` / `import "mod"` → the `source` string
            // literal, stripped of its surrounding quotes.
            if let Some(s) = field_text(node, "source", src) {
                let m = s.trim_matches(|c| c == '"' || c == '\'' || c == '`');
                if !m.is_empty() {
                    out.push(NamedLine {
                        name: m.to_string(),
                        line,
                    });
                }
            }
        }
    }
}

/// Resolve a call node's callee name. `foo()` → `"foo"`; `obj.method()` /
/// `a::b::read()` → `"method"` / `"read"` (the trailing identifier, the useful
/// edge target). Returns `None` for calls whose target isn't a name
/// (`f()()`, `arr[0]()`) — those yield no meaningful name-level edge. The
/// `function` field is shared across Python `call` and JS/TS/Rust
/// `call_expression`, so this is language-agnostic.
fn callee_name(call: Node, src: &[u8]) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    match f.kind() {
        // A bare literal/index/paren callee has no name-level target.
        "subscript_expression"
        | "index_expression"
        | "parenthesized_expression"
        | "call_expression"
        | "call" => None,
        _ => final_identifier(f, src),
    }
}

/// A JSX tag name (`<Foo/>` → `Foo`) if it names a *component* — i.e. its first
/// character is uppercase (the React convention that distinguishes a component
/// from a host element like `<div>`). Lowercase host tags return `None` so they
/// don't pollute the call graph. The captured node is the tag's `identifier`.
fn jsx_component_name(name: Node, src: &[u8]) -> Option<String> {
    let text = name.utf8_text(src).ok()?;
    match text.chars().next() {
        Some(c) if c.is_uppercase() => Some(text.to_string()),
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

    /// Extract entities for an arbitrary language, inferring it from `ext`.
    fn extract_ext(src: &str, ext: &str) -> Value {
        let f = temp_source(src, ext);
        extract_entities(f.path().to_str().unwrap(), None).unwrap()
    }

    fn names_of<'a>(v: &'a Value, key: &str, field: &str) -> Vec<&'a str> {
        v[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e[field].as_str().unwrap())
            .collect()
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
        let methods: Vec<&str> = w["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert_eq!(methods, vec!["__init__", "render"]);
        let bases: Vec<&str> = w["bases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        assert_eq!(bases, vec!["Base", "mixins.Loud"]);
        // Methods must NOT leak into top-level functions.
        assert!(v["functions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn extracts_imports_both_forms() {
        let src = "import os\nimport os.path as p\nimport sys, json\nfrom collections import OrderedDict\nfrom . import sibling\n";
        let v = extract(src);
        let imports: Vec<&str> = v["imports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["module"].as_str().unwrap())
            .collect();
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
        let top = calls
            .iter()
            .find(|c| c["callee"] == "top_level_call")
            .unwrap();
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
        assert_eq!(
            v["counts"]["functions"],
            v["functions"].as_array().unwrap().len()
        );
        assert_eq!(
            v["counts"]["classes"],
            v["classes"].as_array().unwrap().len()
        );
        assert_eq!(
            v["counts"]["imports"],
            v["imports"].as_array().unwrap().len()
        );
    }

    #[test]
    fn unknown_language_is_rejected() {
        // `.hs` (Haskell) isn't linked → no grammar to infer.
        let f = temp_source("main = putStrLn \"hi\"\n", "hs");
        let err = extract_entities(f.path().to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.code, "unknown_language");
    }

    #[test]
    fn explicit_unknown_language_is_rejected() {
        let f = temp_source("x = 1\n", "py");
        let err = extract_entities(f.path().to_str().unwrap(), Some("haskell")).unwrap_err();
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

    // ── Rust ────────────────────────────────────────────────────────────────

    #[test]
    fn rust_extracts_functions_struct_methods_and_impl_trait_base() {
        let src = "use std::collections::HashMap;\n\
                   \nfn free() {\n    helper();\n}\n\
                   \nstruct Widget {\n    x: u32,\n}\n\
                   \ntrait Render {\n    fn render(&self);\n}\n\
                   \nimpl Render for Widget {\n    fn render(&self) {\n        self.draw();\n    }\n}\n";
        let v = extract_ext(src, "rs");
        assert_eq!(v["language"], "rust");

        // Free fn is top-level; the impl method is NOT.
        let fns = names_of(&v, "functions", "name");
        assert!(fns.contains(&"free"), "free fn missing: {fns:?}");
        assert!(!fns.contains(&"render"), "method leaked into functions");

        // `impl Render for Widget` → a Widget class carrying `render`, with
        // `Render` as an INHERITS base.
        let classes = v["classes"].as_array().unwrap();
        let widget_impl = classes
            .iter()
            .find(|c| c["name"] == "Widget" && !c["methods"].as_array().unwrap().is_empty())
            .expect("impl Widget with methods");
        let methods: Vec<&str> = widget_impl["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert_eq!(methods, vec!["render"]);
        let bases: Vec<&str> = widget_impl["bases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        assert_eq!(bases, vec!["Render"]);

        // `use std::collections::HashMap;` → module path `std::collections`.
        assert_eq!(names_of(&v, "imports", "module"), vec!["std::collections"]);

        // free() → helper(); render() → self.draw() attributed to render.
        let calls = v["calls"].as_array().unwrap();
        let helper = calls.iter().find(|c| c["callee"] == "helper").unwrap();
        assert_eq!(helper["caller"], "free");
        let draw = calls.iter().find(|c| c["callee"] == "draw").unwrap();
        assert_eq!(draw["caller"], "render");
    }

    // ── JavaScript ───────────────────────────────────────────────────────────

    #[test]
    fn javascript_extracts_functions_class_and_es_imports() {
        let src = "import { mount } from './dom.js';\n\
                   \nexport function boot() {\n    mount();\n}\n\
                   \nclass View extends Base {\n  render() {\n    draw();\n  }\n}\n";
        let v = extract_ext(src, "js");
        assert_eq!(v["language"], "javascript");

        // `export function boot` is still a top-level function.
        assert!(names_of(&v, "functions", "name").contains(&"boot"));

        let classes = v["classes"].as_array().unwrap();
        assert_eq!(classes[0]["name"], "View");
        assert_eq!(names_of(&v, "classes", "name"), vec!["View"]);
        let methods: Vec<&str> = classes[0]["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert_eq!(methods, vec!["render"]);
        let bases: Vec<&str> = classes[0]["bases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        assert_eq!(bases, vec!["Base"]);

        // ES import source string, quotes stripped.
        assert_eq!(names_of(&v, "imports", "module"), vec!["./dom.js"]);

        let calls = v["calls"].as_array().unwrap();
        assert_eq!(
            calls.iter().find(|c| c["callee"] == "mount").unwrap()["caller"],
            "boot"
        );
        assert_eq!(
            calls.iter().find(|c| c["callee"] == "draw").unwrap()["caller"],
            "render"
        );
    }

    // ── TypeScript ───────────────────────────────────────────────────────────

    #[test]
    fn typescript_extracts_class_interface_and_implements_base() {
        let src = "import type { Opts } from './opts';\n\
                   \ninterface Shape {\n  area(): number;\n}\n\
                   \nexport function make(): void {\n  build();\n}\n\
                   \nclass Circle extends Base implements Shape {\n  area(): number {\n    return compute();\n  }\n}\n";
        let v = extract_ext(src, "ts");
        assert_eq!(v["language"], "typescript");

        assert!(names_of(&v, "functions", "name").contains(&"make"));

        let classes = v["classes"].as_array().unwrap();
        // Interface Shape carries its method signature.
        let shape = classes
            .iter()
            .find(|c| c["name"] == "Shape")
            .expect("interface Shape");
        assert_eq!(shape["methods"].as_array().unwrap()[0], "area");
        // Circle extends Base + implements Shape → both are bases.
        let circle = classes
            .iter()
            .find(|c| c["name"] == "Circle")
            .expect("class Circle");
        let bases: Vec<&str> = circle["bases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        assert!(bases.contains(&"Base"), "extends base missing: {bases:?}");
        assert!(
            bases.contains(&"Shape"),
            "implements base missing: {bases:?}"
        );
        assert_eq!(circle["methods"].as_array().unwrap()[0], "area");

        assert_eq!(names_of(&v, "imports", "module"), vec!["./opts"]);

        let calls = v["calls"].as_array().unwrap();
        assert_eq!(
            calls.iter().find(|c| c["callee"] == "build").unwrap()["caller"],
            "make"
        );
        assert_eq!(
            calls.iter().find(|c| c["callee"] == "compute").unwrap()["caller"],
            "area"
        );
    }

    // ── TSX (TypeScript + JSX) ────────────────────────────────────────────────

    #[test]
    fn tsx_extracts_component_function_class_imports_and_jsx_calls() {
        let src = "import { Child } from './child';\n\
                   \nexport function App(): JSX.Element {\n\
                   \n  return (\n    <div className=\"root\">\n      <Child />\n      <span>hi</span>\n    </div>\n  );\n}\n\
                   \nclass Panel extends Component {\n  render() {\n    return <Child />;\n  }\n}\n";
        let v = extract_ext(src, "tsx");
        assert_eq!(v["language"], "tsx");

        // The React component is a top-level function.
        assert!(names_of(&v, "functions", "name").contains(&"App"));

        // The class component carries its method + `extends` base.
        let classes = v["classes"].as_array().unwrap();
        let panel = classes
            .iter()
            .find(|c| c["name"] == "Panel")
            .expect("class Panel");
        let bases: Vec<&str> = panel["bases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect();
        assert_eq!(bases, vec!["Component"]);
        assert_eq!(panel["methods"].as_array().unwrap()[0], "render");

        // ES import source string, quotes stripped.
        assert_eq!(names_of(&v, "imports", "module"), vec!["./child"]);

        // JSX usage → CALLS edges. `<Child/>` in App is attributed to App; the
        // one in Panel.render to render. Host tags (`div`, `span`) are filtered.
        let calls = v["calls"].as_array().unwrap();
        let child_in_app = calls
            .iter()
            .find(|c| c["callee"] == "Child" && c["caller"] == "App")
            .expect("Child rendered by App");
        assert!(child_in_app["line"].as_u64().unwrap() >= 1);
        assert!(calls
            .iter()
            .any(|c| c["callee"] == "Child" && c["caller"] == "render"));
        // No host element leaked in as a call.
        assert!(calls
            .iter()
            .all(|c| c["callee"] != "div" && c["callee"] != "span"));
    }

    #[test]
    fn markdown_has_no_entity_extraction() {
        // Markdown is chunk-only — extract_entities rejects it.
        let f = temp_source("# Title\n\nbody\n", "md");
        let err = extract_entities(f.path().to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.code, "unsupported_language");
    }
}
