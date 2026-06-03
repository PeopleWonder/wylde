//! `fs_file` + `fs_dir` resources — the filesystem verb migration
//! (consolidation Slice 4, `docs/plans/tool-registry-consolidation.md`
//! §6). Folds the flat `fs.*` + `search.*` named tools into two
//! resource types under the generic verbs:
//!
//! | Verb call | Delegates to |
//! |---|---|
//! | `wylde_list("fs_file", {filter:{path}})` | [`tools_fs::run_list_files`] (files only) |
//! | `wylde_get("fs_file", path)` | [`tools_fs::run_read_file`] |
//! | `wylde_create("fs_file", {body:{path, content}})` | [`tools_fs::run_create_file`] (refuse-overwrite) |
//! | `wylde_update("fs_file", path, {body:{content}\|{old_text,new_text}})` | [`tools_fs::run_write_file`] / [`tools_fs::run_edit_file`] |
//! | `wylde_delete("fs_file", path)` | [`tools_fs::run_delete_file`] |
//! | `wylde_search("fs_file", pattern, {filter:{path, glob, …}})` | [`tools_search::run_code_search`] (content grep) |
//! | `wylde_list("fs_dir", {filter:{path}})` | [`tools_fs::run_list_files`] (dirs only) |
//! | `wylde_create("fs_dir", {body:{path}})` | [`tools_fs::run_make_dir`] (`mkdir -p`) |
//! | `wylde_delete("fs_dir", path, {body:{recursive}})` | [`tools_fs::run_remove_dir`] (`rmdir`) |
//! | `wylde_search("fs_dir", glob, {filter:{path}})` | [`tools_search::run_code_search_files`] (find by name) |
//!
//! ## Adapter pattern — no logic duplication (the memory.rs template)
//!
//! Each [`OpHandler`] reshapes its [`ResourceRequest`] into the `args`
//! object an existing `fs.*` / `search.*` primitive accepts, then calls
//! straight through. The named `read_file` / `write_file` / `code_search`
//! / … tools stay registered and unchanged — both surfaces run in
//! parallel until the Slice-6 cutover behind `WYLDE_HARNESS_VERB_TOOLS`.
//!
//! ## Path safety (plan requirement #4)
//!
//! Neither the Python originals (`Core/harness/tooling/tools/fs/`) nor the
//! Rust port impose a path allow-list or workspace confinement — the
//! harness is a coding agent whose workspace can live anywhere. These
//! adapters delegate to the **same** primitives the named tools use, so
//! the verb surface is never *wider* than the named tools and opens no
//! new hole. The single home for any future confinement guard is
//! [`crate::tooling::tools::fs`] (one chokepoint for every fs primitive),
//! not here.
//!
//! ## CRUD split — create vs update
//!
//! `create` writes a *new* file and refuses to clobber an existing one
//! (`already_exists`); `update` overwrites (`body.content`) or does a
//! literal substring replace (`body.old_text` / `body.new_text`). This is
//! the honest CRUD shape — `create` is not a silent overwrite.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;
use crate::tooling::tools::fs as tools_fs;
use crate::tooling::tools::search as tools_search;

/// Register the `fs_file` and `fs_dir` resources into the built-in
/// registry.
pub fn register_fs_resources(reg: &mut ResourceRegistry) {
    register_fs_file(reg);
    register_fs_dir(reg);
}

// ── fs_file ──────────────────────────────────────────────────────────

fn register_fs_file(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(file_list));
    operations.insert(ResourceOp::Get, op_handler(file_get));
    operations.insert(ResourceOp::Create, op_handler(file_create));
    operations.insert(ResourceOp::Update, op_handler(file_update));
    operations.insert(ResourceOp::Delete, op_handler(file_delete));
    operations.insert(ResourceOp::Search, op_handler(file_search));

    reg.register_builtin(ResourceDefinition {
        resource_type: "fs_file",
        display_name: "File",
        description: "A file on the local filesystem. list (files in a directory), \
                      get (read contents), create (write new), update (overwrite or \
                      substring-edit), delete, search (regex content grep).",
        scope: Scope::Global,
        identifier_fields: &["path"],
        filter_fields: &["path", "glob", "case_insensitive", "max_count"],
        operations,
        destructive_ops: &[ResourceOp::Create, ResourceOp::Update, ResourceOp::Delete],
        describe: describe_value(describe_fs_file),
    });
}

/// `wylde_list("fs_file", {filter:{path}})` → directory listing, files
/// only. The directory comes from `filter.path` (falling back to
/// `resource_id`, then `"."`).
fn file_list(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let path = dir_path(&req);
    async move {
        let listing = tools_fs::run_list_files(json!({ "path": path })).await?;
        Ok(filter_listing_by_type(listing, "file"))
    }
}

/// `wylde_get("fs_file", path)` → read file contents.
fn file_get(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let path = target_path(&req);
    async move {
        match path {
            Some(p) => tools_fs::run_read_file(json!({ "path": p })).await,
            None => Ok(missing("wylde_get(\"fs_file\", …) requires 'resource_id' (the path)")),
        }
    }
}

/// `wylde_create("fs_file", {body:{path, content}})` → write a new file
/// (refuses to clobber an existing one).
fn file_create(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.body);
    // resource_id may carry the path instead of body.path.
    if let Some(id) = req.resource_id {
        args.entry("path").or_insert(json!(id));
    }
    async move { tools_fs::run_create_file(Value::Object(args)).await }
}

/// `wylde_update("fs_file", path, {body:…})` → overwrite (`content`) or
/// literal substring replace (`old_text`/`new_text`). The verb's
/// `resource_id` is the path.
fn file_update(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.body);
    if let Some(id) = req.resource_id {
        args.insert("path".into(), json!(id));
    }
    let has_path = args.get("path").and_then(Value::as_str).is_some();
    let has_old_text = args.contains_key("old_text");
    let has_content = args.contains_key("content");
    async move {
        if !has_path {
            return Ok(missing(
                "wylde_update(\"fs_file\", …) requires 'resource_id' (the path)",
            ));
        }
        if has_old_text {
            // Substring edit — needs old_text (+ optional new_text).
            tools_fs::run_edit_file(Value::Object(args)).await
        } else if has_content {
            // Whole-file overwrite.
            tools_fs::run_write_file(Value::Object(args)).await
        } else {
            Ok(json!({
                "status": "error",
                "error": "wylde_update(\"fs_file\", …) requires body.content (overwrite) \
                          or body.old_text (+body.new_text, substring replace)",
            }))
        }
    }
}

/// `wylde_delete("fs_file", path)` → remove one file.
fn file_delete(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let path = target_path(&req);
    async move {
        match path {
            Some(p) => tools_fs::run_delete_file(json!({ "path": p })).await,
            None => Ok(missing("wylde_delete(\"fs_file\", …) requires 'resource_id' (the path)")),
        }
    }
}

/// `wylde_search("fs_file", pattern, {filter:{path, glob, …}})` → regex
/// content grep. The verb's `query` is the regex `pattern`; `filter`
/// carries `path` / `glob` / `case_insensitive` / `max_count`.
fn file_search(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(q) = req.query {
        args.insert("pattern".into(), json!(q));
    }
    if let Some(l) = req.limit {
        args.insert("max_count".into(), json!(l));
    }
    async move {
        if args.get("pattern").and_then(Value::as_str).is_none() {
            return Ok(missing(
                "wylde_search(\"fs_file\", …) requires 'query' (the regex pattern)",
            ));
        }
        tools_search::run_code_search(Value::Object(args)).await
    }
}

// ── fs_dir ───────────────────────────────────────────────────────────

fn register_fs_dir(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::List, op_handler(dir_list));
    operations.insert(ResourceOp::Create, op_handler(dir_create));
    operations.insert(ResourceOp::Delete, op_handler(dir_delete));
    operations.insert(ResourceOp::Search, op_handler(dir_search));

    reg.register_builtin(ResourceDefinition {
        resource_type: "fs_dir",
        display_name: "Directory",
        description: "A directory on the local filesystem. list (child subdirectories), \
                      create (mkdir -p), delete (rmdir; recursive opt-in), search (find \
                      files by name glob).",
        scope: Scope::Global,
        identifier_fields: &["path"],
        filter_fields: &["path", "max_count", "recursive"],
        operations,
        destructive_ops: &[ResourceOp::Create, ResourceOp::Delete],
        describe: describe_value(describe_fs_dir),
    });
}

/// `wylde_list("fs_dir", {filter:{path}})` → child directories of a path.
fn dir_list(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let path = dir_path(&req);
    async move {
        let listing = tools_fs::run_list_files(json!({ "path": path })).await?;
        Ok(filter_listing_by_type(listing, "dir"))
    }
}

/// `wylde_create("fs_dir", {body:{path}})` → `mkdir -p`.
fn dir_create(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.body);
    if let Some(id) = req.resource_id {
        args.entry("path").or_insert(json!(id));
    }
    async move { tools_fs::run_make_dir(Value::Object(args)).await }
}

/// `wylde_delete("fs_dir", path, {body:{recursive}})` → `rmdir` (empty by
/// default; `recursive=true` removes contents).
fn dir_delete(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let path = target_path(&req);
    // `recursive` may travel in either body or filter.
    let recursive = bool_field(&req.body, "recursive").or_else(|| bool_field(&req.filter, "recursive"));
    async move {
        match path {
            Some(p) => {
                let mut args = Map::new();
                args.insert("path".into(), json!(p));
                if let Some(r) = recursive {
                    args.insert("recursive".into(), json!(r));
                }
                tools_fs::run_remove_dir(Value::Object(args)).await
            }
            None => Ok(missing("wylde_delete(\"fs_dir\", …) requires 'resource_id' (the path)")),
        }
    }
}

/// `wylde_search("fs_dir", glob, {filter:{path, max_count}})` → find files
/// by name glob. The verb's `query` is the glob.
fn dir_search(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let mut args = as_object(req.filter);
    if let Some(q) = req.query {
        args.insert("glob".into(), json!(q));
    }
    if let Some(l) = req.limit {
        args.insert("max_count".into(), json!(l));
    }
    async move {
        if args.get("glob").and_then(Value::as_str).is_none() {
            return Ok(missing(
                "wylde_search(\"fs_dir\", …) requires 'query' (the filename glob)",
            ));
        }
        tools_search::run_code_search_files(Value::Object(args)).await
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Coerce a `Value` into an owned object map — `Null` / non-objects
/// become an empty map so handlers always see a well-formed `args`.
fn as_object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

/// The directory a `list` targets: `filter.path`, else `resource_id`,
/// else `"."` (matching `run_list_files`'s default).
fn dir_path(req: &ResourceRequest) -> String {
    str_field(&req.filter, "path")
        .or_else(|| req.resource_id.clone())
        .unwrap_or_else(|| ".".to_string())
}

/// The path a `get`/`update`/`delete` targets: `resource_id`, else
/// `body.path`. Returns `None` when neither is present.
fn target_path(req: &ResourceRequest) -> Option<String> {
    req.resource_id.clone().or_else(|| str_field(&req.body, "path"))
}

/// Keep only listing entries of `want_type` (`"file"` / `"dir"`),
/// recomputing `count`. Passes through error envelopes untouched.
fn filter_listing_by_type(mut listing: Value, want_type: &str) -> Value {
    if listing.get("status").and_then(Value::as_str) != Some("success") {
        return listing;
    }
    if let Some(files) = listing.get("files").and_then(Value::as_array) {
        let kept: Vec<Value> = files
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some(want_type))
            .cloned()
            .collect();
        let count = kept.len();
        if let Some(obj) = listing.as_object_mut() {
            obj.insert("files".into(), Value::Array(kept));
            obj.insert("count".into(), json!(count));
        }
    }
    listing
}

/// A `status: "error"` envelope for a missing required argument.
fn missing(msg: &str) -> Value {
    json!({ "status": "error", "error": msg })
}

// ── describe() ───────────────────────────────────────────────────────

fn describe_fs_file() -> Value {
    json!({
        "resource_type": "fs_file",
        "display_name": "File",
        "description": "A file on the local filesystem.",
        "scope": "global",
        "identifier_fields": ["path"],
        "filter_fields": ["path", "glob", "case_insensitive", "max_count"],
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "List files in a directory (non-recursive; directories \
                                excluded — use fs_dir list for those).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Directory to list (default '.')"}
                            }
                        }
                    }
                }
            },
            "get": {
                "verb": "wylde_get",
                "destructive": false,
                "description": "Read a file's text contents (up to 100 KiB; truncated flag set if larger).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "File path"}
                    },
                    "required": ["resource_id"]
                }
            },
            "create": {
                "verb": "wylde_create",
                "destructive": true,
                "description": "Write a new file (creates parent dirs). Errors with \
                                code 'already_exists' if the path exists — use update to overwrite.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File path to create"},
                                "content": {"type": "string", "description": "Text to write"}
                            },
                            "required": ["path", "content"]
                        }
                    }
                }
            },
            "update": {
                "verb": "wylde_update",
                "destructive": true,
                "description": "Overwrite a file (body.content) or replace every occurrence \
                                of a literal substring (body.old_text + body.new_text).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "File path"},
                        "body": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "New full contents (overwrite mode)"},
                                "old_text": {"type": "string", "description": "Literal text to replace (substring-edit mode)"},
                                "new_text": {"type": "string", "description": "Replacement text (substring-edit mode)"}
                            }
                        }
                    },
                    "required": ["resource_id"]
                }
            },
            "delete": {
                "verb": "wylde_delete",
                "destructive": true,
                "description": "Delete a single file. Errors if the path is a directory.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "File path"}
                    },
                    "required": ["resource_id"]
                }
            },
            "search": {
                "verb": "wylde_search",
                "destructive": false,
                "description": "Regex content grep across files. query is the regex pattern; \
                                filter narrows by path/glob.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Regex pattern to search for"},
                        "filter": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Directory to search (default '.')"},
                                "glob": {"type": "string", "description": "Filename glob filter, e.g. '*.rs'"},
                                "case_insensitive": {"type": "boolean", "description": "Case-insensitive match"},
                                "max_count": {"type": "number", "description": "Max matches (default 500)"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max matches; overlays filter.max_count"}
                    },
                    "required": ["query"]
                }
            }
        }
    })
}

fn describe_fs_dir() -> Value {
    json!({
        "resource_type": "fs_dir",
        "display_name": "Directory",
        "description": "A directory on the local filesystem.",
        "scope": "global",
        "identifier_fields": ["path"],
        "filter_fields": ["path", "max_count", "recursive"],
        "operations": {
            "list": {
                "verb": "wylde_list",
                "destructive": false,
                "description": "List child subdirectories of a directory (non-recursive).",
                "schema": {
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Directory to list (default '.')"}
                            }
                        }
                    }
                }
            },
            "create": {
                "verb": "wylde_create",
                "destructive": true,
                "description": "Create a directory and any missing parents (mkdir -p). Idempotent.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Directory path to create"}
                            },
                            "required": ["path"]
                        }
                    }
                }
            },
            "delete": {
                "verb": "wylde_delete",
                "destructive": true,
                "description": "Remove a directory (rmdir). Empty-only by default; pass \
                                body.recursive=true to remove a non-empty tree.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "resource_id": {"type": "string", "description": "Directory path"},
                        "body": {
                            "type": "object",
                            "properties": {
                                "recursive": {"type": "boolean", "description": "Remove contents too (default false)"}
                            }
                        }
                    },
                    "required": ["resource_id"]
                }
            },
            "search": {
                "verb": "wylde_search",
                "destructive": false,
                "description": "Find files by filename glob (skips noise dirs). query is the glob.",
                "schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Filename glob, e.g. '*.py', 'src/**/*.tsx'"},
                        "filter": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Directory to search (default '.')"},
                                "max_count": {"type": "number", "description": "Max files (default 500)"}
                            }
                        },
                        "limit": {"type": "number", "description": "Max files; overlays filter.max_count"}
                    },
                    "required": ["query"]
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::resource::ToolsetFilter;
    use tempfile::tempdir;
    use tokio::fs as tokio_fs;

    fn cfg() -> &'static crate::config::Config {
        Box::leak(Box::new(crate::config::Config::default_for_tests()))
    }

    /// A registry with only the fs resources — exercises the exact wiring
    /// `register_fs_resources` produces, in isolation.
    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_fs_resources(&mut r);
        r
    }

    /// Run an op through the registered `OpHandler` (the real verb-layer
    /// dispatch path).
    async fn dispatch(rt: &str, op: ResourceOp, req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup(rt).unwrap_or_else(|| panic!("{rt} registered"));
        let handler = def.operations.get(&op).expect("op registered").clone();
        let ctx = ToolContext::for_op(rt, op, req.resource_id.clone());
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    // ── registration / metadata ─────────────────────────────────────

    #[test]
    fn registers_both_resources() {
        let r = reg();
        assert!(r.lookup("fs_file").is_some());
        assert!(r.lookup("fs_dir").is_some());
    }

    #[test]
    fn fs_file_supports_full_crud_plus_search() {
        let r = reg();
        let def = r.lookup("fs_file").unwrap();
        assert_eq!(
            def.supported_ops(),
            vec![
                ResourceOp::List,
                ResourceOp::Get,
                ResourceOp::Create,
                ResourceOp::Update,
                ResourceOp::Delete,
                ResourceOp::Search,
            ]
        );
    }

    #[test]
    fn fs_dir_supports_list_create_delete_search() {
        let r = reg();
        let def = r.lookup("fs_dir").unwrap();
        assert_eq!(
            def.supported_ops(),
            vec![
                ResourceOp::List,
                ResourceOp::Create,
                ResourceOp::Delete,
                ResourceOp::Search,
            ]
        );
    }

    #[test]
    fn destructive_classification_matches_named_tools() {
        let r = reg();
        let file = r.lookup("fs_file").unwrap();
        assert!(file.is_destructive(ResourceOp::Create));
        assert!(file.is_destructive(ResourceOp::Update));
        assert!(file.is_destructive(ResourceOp::Delete));
        assert!(!file.is_destructive(ResourceOp::Get));
        assert!(!file.is_destructive(ResourceOp::List));
        assert!(!file.is_destructive(ResourceOp::Search));
        let dir = r.lookup("fs_dir").unwrap();
        assert!(dir.is_destructive(ResourceOp::Create));
        assert!(dir.is_destructive(ResourceOp::Delete));
        assert!(!dir.is_destructive(ResourceOp::List));
        assert!(!dir.is_destructive(ResourceOp::Search));
    }

    #[test]
    fn both_resources_are_searchable_at_global_scope() {
        let r = reg();
        let filter = ToolsetFilter::all();
        let mut types = r.searchable_types(&filter);
        types.sort();
        assert_eq!(types, vec!["fs_dir".to_string(), "fs_file".to_string()]);
    }

    #[test]
    fn describe_lists_all_operations() {
        let f = describe_fs_file();
        let ops = f["operations"].as_object().unwrap();
        for op in ["list", "get", "create", "update", "delete", "search"] {
            assert!(ops.contains_key(op), "fs_file describe missing {op}");
        }
        assert_eq!(ops["create"]["destructive"], true);
        assert_eq!(ops["get"]["destructive"], false);
        let d = describe_fs_dir();
        let dops = d["operations"].as_object().unwrap();
        for op in ["list", "create", "delete", "search"] {
            assert!(dops.contains_key(op), "fs_dir describe missing {op}");
        }
    }

    // ── fs_file round-trips via the verb path ───────────────────────

    #[tokio::test]
    async fn fs_file_create_list_get_delete_cycle() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("note.txt").display().to_string();

        // create
        let created = dispatch(
            "fs_file",
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"path": path, "content": "hello verb"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(created["status"], "success");

        // list — the new file shows up, files only
        let listed = dispatch(
            "fs_file",
            ResourceOp::List,
            ResourceRequest {
                filter: json!({"path": dir.path().display().to_string()}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(listed["status"], "success");
        let names: Vec<&str> = listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"note.txt"));
        // every entry is a file
        assert!(listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["type"] == "file"));

        // get
        let got = dispatch(
            "fs_file",
            ResourceOp::Get,
            ResourceRequest {
                resource_id: Some(path.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got["status"], "success");
        assert_eq!(got["content"], "hello verb");

        // delete
        let deleted = dispatch(
            "fs_file",
            ResourceOp::Delete,
            ResourceRequest {
                resource_id: Some(path.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(deleted["status"], "success");
        assert!(!tokio_fs::try_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn fs_file_create_refuses_overwrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt").display().to_string();
        tokio_fs::write(&path, "first").await.unwrap();
        let out = dispatch(
            "fs_file",
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"path": path, "content": "second"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "already_exists");
    }

    #[tokio::test]
    async fn fs_file_update_overwrite_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt").display().to_string();
        tokio_fs::write(&path, "old").await.unwrap();
        let out = dispatch(
            "fs_file",
            ResourceOp::Update,
            ResourceRequest {
                resource_id: Some(path.clone()),
                body: json!({"content": "new"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(tokio_fs::read_to_string(&path).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn fs_file_update_substring_edit_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt").display().to_string();
        tokio_fs::write(&path, "foo bar foo").await.unwrap();
        let out = dispatch(
            "fs_file",
            ResourceOp::Update,
            ResourceRequest {
                resource_id: Some(path.clone()),
                body: json!({"old_text": "foo", "new_text": "qux"}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(out["replacements"], 2);
        assert_eq!(tokio_fs::read_to_string(&path).await.unwrap(), "qux bar qux");
    }

    #[tokio::test]
    async fn fs_file_update_without_content_or_old_text_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt").display().to_string();
        tokio_fs::write(&path, "x").await.unwrap();
        let out = dispatch(
            "fs_file",
            ResourceOp::Update,
            ResourceRequest {
                resource_id: Some(path),
                body: json!({}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
    }

    #[tokio::test]
    async fn fs_file_get_without_id_errors() {
        let out = dispatch("fs_file", ResourceOp::Get, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
    }

    #[tokio::test]
    async fn fs_file_search_greps_content() {
        let dir = tempdir().unwrap();
        tokio_fs::write(dir.path().join("a.txt"), "needle here\nother\n")
            .await
            .unwrap();
        let out = dispatch(
            "fs_file",
            ResourceOp::Search,
            ResourceRequest {
                query: Some("needle".into()),
                filter: json!({"path": dir.path().display().to_string()}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(out["count"], 1);
    }

    #[tokio::test]
    async fn fs_file_search_without_query_errors() {
        let out = dispatch("fs_file", ResourceOp::Search, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
    }

    // ── fs_dir round-trips via the verb path ────────────────────────

    #[tokio::test]
    async fn fs_dir_create_list_delete_cycle() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("nested/child").display().to_string();

        // create (mkdir -p)
        let created = dispatch(
            "fs_dir",
            ResourceOp::Create,
            ResourceRequest {
                body: json!({"path": sub}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(created["status"], "success");

        // list — child dirs only
        let listed = dispatch(
            "fs_dir",
            ResourceOp::List,
            ResourceRequest {
                filter: json!({"path": dir.path().display().to_string()}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(listed["status"], "success");
        let names: Vec<&str> = listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["nested"]);
        assert!(listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["type"] == "dir"));

        // delete recursively (nested is non-empty)
        let deleted = dispatch(
            "fs_dir",
            ResourceOp::Delete,
            ResourceRequest {
                resource_id: Some(dir.path().join("nested").display().to_string()),
                body: json!({"recursive": true}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(deleted["status"], "success");
        assert!(!tokio_fs::try_exists(dir.path().join("nested")).await.unwrap());
    }

    #[tokio::test]
    async fn fs_dir_delete_nonempty_without_recursive_errors() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("full");
        tokio_fs::create_dir(&sub).await.unwrap();
        tokio_fs::write(sub.join("f.txt"), "x").await.unwrap();
        let out = dispatch(
            "fs_dir",
            ResourceOp::Delete,
            ResourceRequest {
                resource_id: Some(sub.display().to_string()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "error");
        assert!(tokio_fs::try_exists(&sub).await.unwrap());
    }

    #[tokio::test]
    async fn fs_dir_search_finds_files_by_glob() {
        let dir = tempdir().unwrap();
        tokio_fs::write(dir.path().join("a.rs"), "").await.unwrap();
        tokio_fs::write(dir.path().join("b.py"), "").await.unwrap();
        let out = dispatch(
            "fs_dir",
            ResourceOp::Search,
            ResourceRequest {
                query: Some("*.rs".into()),
                filter: json!({"path": dir.path().display().to_string()}),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out["status"], "success");
        assert_eq!(out["count"], 1);
    }

    #[tokio::test]
    async fn fs_dir_delete_without_id_errors() {
        let out = dispatch("fs_dir", ResourceOp::Delete, ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
    }
}
