//! `fs.*` tools — Rust port of `Core/harness/tooling/tools/fs/`.
//!
//! Four entrypoints: `read_file` (non-destructive read), `list_files`
//! (non-destructive directory enumeration), `write_file` (destructive
//! create/overwrite), `edit_file` (destructive substring replace).
//!
//! Wire shape matches the Python tools exactly — the dispatcher feeds
//! the result straight into the LLM's tool message, and existing
//! conversation histories assume keys like `status: "success"`,
//! `content`, `files`, etc.

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::fs as tokio_fs;
use wylde_shared::ipc::IpcError;

use crate::tooling::registry::{entry_active, param, param_default, Registry};

/// Cap on file content returned by `read_file`. Mirrors Python's
/// `_CONTENT_CAP = 100_000` in `read_file.py`.
const CONTENT_CAP: usize = 100_000;

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "read_file",
        "fs.read_file",
        "fs",
        "Read the text contents of a file. Returns up to 100 KiB; sets \
         `truncated=true` if the file was larger.",
        vec![param("path", "string", true, "Path to the file")],
        false,
        |args, _| async move { run_read_file(args).await },
    ));

    reg.insert(entry_active(
        "list_files",
        "fs.list_files",
        "fs",
        "List the immediate contents of a directory. Returns name, type \
         (file/dir), and size for each entry. Non-recursive.",
        vec![param_default(
            "path",
            "string",
            "Directory to list",
            json!("."),
        )],
        false,
        |args, _| async move { run_list_files(args).await },
    ));

    reg.insert(entry_active(
        "write_file",
        "fs.write_file",
        "fs",
        "Write text to a file. Creates missing parent directories. \
         Overwrites any existing content.",
        vec![
            param("path", "string", true, "Path to the file"),
            param("content", "string", true, "Text to write"),
        ],
        true,
        |args, _| async move { run_write_file(args).await },
    ));

    reg.insert(entry_active(
        "edit_file",
        "fs.edit_file",
        "fs",
        "Replace every occurrence of a literal substring in a file. \
         Errors if the pattern is not found. Returns the number of \
         replacements applied.",
        vec![
            param("path", "string", true, "Path to the file"),
            param("old_text", "string", true, "Literal text to replace"),
            param("new_text", "string", true, "Replacement text"),
        ],
        true,
        |args, _| async move { run_edit_file(args).await },
    ));
}

pub(crate) async fn run_read_file(args: Value) -> Result<Value, IpcError> {
    let path_str = match str_field(&args, "path") {
        Some(s) => s,
        None => {
            return Ok(json!({
                "status": "error",
                "error": "'path' is required",
            }));
        }
    };
    let path = PathBuf::from(&path_str);
    if !tokio_fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(json!({
            "status": "error",
            "error": format!("file not found: {path_str}"),
            "code": "not_found",
        }));
    }
    let bytes = match tokio_fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(json!({
                "status": "error",
                "error": format!("{e}"),
            }));
        }
    };
    // UTF-8 with replacement — matches Python's `errors="replace"`.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let total = text.chars().count();
    let truncated = text.len() > CONTENT_CAP;
    let content: String = if truncated {
        text.chars().take(CONTENT_CAP).collect()
    } else {
        text
    };
    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "content": content,
        "size": total,
        "truncated": truncated,
    }))
}

pub(crate) async fn run_list_files(args: Value) -> Result<Value, IpcError> {
    let path_str = str_field(&args, "path").unwrap_or_else(|| ".".to_string());
    let path = PathBuf::from(&path_str);
    if !tokio_fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(json!({
            "status": "error",
            "error": format!("path not found: {path_str}"),
            "code": "not_found",
        }));
    }
    let meta = match tokio_fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => {
            return Ok(json!({
                "status": "error",
                "error": format!("{e}"),
            }));
        }
    };
    if !meta.is_dir() {
        return Ok(json!({
            "status": "error",
            "error": format!("not a directory: {path_str}"),
        }));
    }

    let mut rd = match tokio_fs::read_dir(&path).await {
        Ok(rd) => rd,
        Err(e) => {
            return Ok(json!({
                "status": "error",
                "error": format!("{e}"),
            }));
        }
    };
    let mut entries: Vec<(String, bool, Option<u64>)> = Vec::new();
    while let Ok(Some(child)) = rd.next_entry().await {
        let name = child.file_name().to_string_lossy().into_owned();
        let ft = match child.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let is_dir = ft.is_dir();
        let size = if is_dir {
            None
        } else {
            child.metadata().await.ok().map(|m| m.len())
        };
        entries.push((name, is_dir, size));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let files: Vec<Value> = entries
        .iter()
        .map(|(name, is_dir, size)| {
            json!({
                "name": name,
                "type": if *is_dir { "dir" } else { "file" },
                "size": size,
            })
        })
        .collect();

    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "count": files.len(),
        "files": files,
    }))
}

pub(crate) async fn run_write_file(args: Value) -> Result<Value, IpcError> {
    let Some(path_str) = str_field(&args, "path") else {
        return Ok(json!({
            "status": "error",
            "error": "'path' is required",
        }));
    };
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    let path = PathBuf::from(&path_str);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio_fs::create_dir_all(parent).await {
                return Ok(json!({
                    "status": "error",
                    "error": format!("create_dir_all: {e}"),
                }));
            }
        }
    }
    if let Err(e) = tokio_fs::write(&path, &content).await {
        return Ok(json!({
            "status": "error",
            "error": format!("{e}"),
        }));
    }
    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "bytes_written": content.len(),
    }))
}

pub(crate) async fn run_edit_file(args: Value) -> Result<Value, IpcError> {
    let Some(path_str) = str_field(&args, "path") else {
        return Ok(json!({
            "status": "error",
            "error": "'path' is required",
        }));
    };
    let Some(old_text) = args.get("old_text").and_then(Value::as_str) else {
        return Ok(json!({
            "status": "error",
            "error": "'old_text' is required",
        }));
    };
    let new_text = args.get("new_text").and_then(Value::as_str).unwrap_or("");
    let path = PathBuf::from(&path_str);
    if !tokio_fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(json!({
            "status": "error",
            "error": format!("file not found: {path_str}"),
            "code": "not_found",
        }));
    }
    let bytes = match tokio_fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(json!({
                "status": "error",
                "error": format!("{e}"),
            }));
        }
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if !text.contains(old_text) {
        return Ok(json!({
            "status": "error",
            "error": "'old_text' not found in file",
            "code": "not_found",
        }));
    }
    let replaced = text.replace(old_text, new_text);
    let count = text.matches(old_text).count();
    if let Err(e) = tokio_fs::write(&path, &replaced).await {
        return Ok(json!({
            "status": "error",
            "error": format!("{e}"),
        }));
    }
    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "replacements": count,
    }))
}

// ── verb-layer primitives ───────────────────────────────────────────
//
// The four handlers above are the named `fs.*` tools. The four below
// back the `fs_file` / `fs_dir` verb resources (consolidation Slice 4,
// `docs/plans/tool-registry-consolidation.md`). They have no named-tool
// twin — `create` (write-new, refuse-overwrite), file `delete`, dir
// `create` (mkdir -p), dir `delete` (rmdir) are operations the resource
// surface introduces. They live here so every fs primitive — and so any
// future path-confinement guard — has a single home, and the resource
// OpHandlers stay pure request-reshaping adapters (the memory.rs pattern).
//
// Path handling matches the named tools exactly: no allow-list, no
// workspace confinement (neither the Python originals nor the Rust port
// imposed one — the harness is a coding agent whose workspace may live
// anywhere). The verb surface is therefore never *wider* than the named
// tools; it opens no new hole.

/// `fs_file` create — write a new file, refusing to clobber an existing
/// one. This is the write-new half of the CRUD split (`update` overwrites;
/// `create` does not). Creates missing parent directories like
/// [`run_write_file`].
pub(crate) async fn run_create_file(args: Value) -> Result<Value, IpcError> {
    let Some(path_str) = str_field(&args, "path") else {
        return Ok(json!({
            "status": "error",
            "error": "'path' is required",
        }));
    };
    let path = PathBuf::from(&path_str);
    if tokio_fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(json!({
            "status": "error",
            "error": format!("file already exists: {path_str} (use update to overwrite)"),
            "code": "already_exists",
        }));
    }
    run_write_file(args).await
}

/// `fs_file` delete — remove a single file. Errors if the path is missing
/// or is a directory (use `fs_dir` delete for directories).
pub(crate) async fn run_delete_file(args: Value) -> Result<Value, IpcError> {
    let Some(path_str) = str_field(&args, "path") else {
        return Ok(json!({
            "status": "error",
            "error": "'path' is required",
        }));
    };
    let path = PathBuf::from(&path_str);
    let meta = match tokio_fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => {
            return Ok(json!({
                "status": "error",
                "error": format!("file not found: {path_str}"),
                "code": "not_found",
            }));
        }
    };
    if meta.is_dir() {
        return Ok(json!({
            "status": "error",
            "error": format!("path is a directory: {path_str} (use fs_dir delete)"),
        }));
    }
    if let Err(e) = tokio_fs::remove_file(&path).await {
        return Ok(json!({
            "status": "error",
            "error": format!("{e}"),
        }));
    }
    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "deleted": true,
    }))
}

/// `fs_dir` create — `mkdir -p`. Idempotent: succeeds if the directory
/// already exists (matches `create_dir_all` semantics).
pub(crate) async fn run_make_dir(args: Value) -> Result<Value, IpcError> {
    let Some(path_str) = str_field(&args, "path") else {
        return Ok(json!({
            "status": "error",
            "error": "'path' is required",
        }));
    };
    let path = PathBuf::from(&path_str);
    if let Err(e) = tokio_fs::create_dir_all(&path).await {
        return Ok(json!({
            "status": "error",
            "error": format!("{e}"),
        }));
    }
    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "created": true,
    }))
}

/// `fs_dir` delete — `rmdir`. Removes an empty directory by default;
/// pass `recursive: true` to remove a directory and its contents.
pub(crate) async fn run_remove_dir(args: Value) -> Result<Value, IpcError> {
    let Some(path_str) = str_field(&args, "path") else {
        return Ok(json!({
            "status": "error",
            "error": "'path' is required",
        }));
    };
    let recursive = args
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = PathBuf::from(&path_str);
    let meta = match tokio_fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => {
            return Ok(json!({
                "status": "error",
                "error": format!("directory not found: {path_str}"),
                "code": "not_found",
            }));
        }
    };
    if !meta.is_dir() {
        return Ok(json!({
            "status": "error",
            "error": format!("not a directory: {path_str}"),
        }));
    }
    let result = if recursive {
        tokio_fs::remove_dir_all(&path).await
    } else {
        tokio_fs::remove_dir(&path).await
    };
    if let Err(e) = result {
        // remove_dir on a non-empty dir surfaces an OS error — pass it
        // through with a hint to use recursive.
        return Ok(json!({
            "status": "error",
            "error": format!("{e}"),
            "hint": if recursive { Value::Null } else { json!("pass recursive=true to remove a non-empty directory") },
        }));
    }
    Ok(json!({
        "status": "success",
        "path": path.display().to_string(),
        "deleted": true,
        "recursive": recursive,
    }))
}

fn str_field(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn read_file_returns_content_and_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "hello").await.unwrap();
        let v = run_read_file(json!({"path": path.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["content"], "hello");
        assert_eq!(v["size"], 5);
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn read_file_errors_on_missing_path_field() {
        let v = run_read_file(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn read_file_returns_not_found_for_missing_file() {
        let v = run_read_file(json!({"path": "no-such-file-honest"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["code"], "not_found");
    }

    #[tokio::test]
    async fn read_file_truncates_past_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let big = "x".repeat(CONTENT_CAP + 100);
        tokio_fs::write(&path, &big).await.unwrap();
        let v = run_read_file(json!({"path": path.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["content"].as_str().unwrap().len(), CONTENT_CAP);
    }

    #[tokio::test]
    async fn list_files_lists_directory_contents_sorted() {
        let dir = tempdir().unwrap();
        tokio_fs::write(dir.path().join("b.txt"), "b")
            .await
            .unwrap();
        tokio_fs::write(dir.path().join("a.txt"), "aa")
            .await
            .unwrap();
        tokio_fs::create_dir(dir.path().join("sub")).await.unwrap();
        let v = run_list_files(json!({"path": dir.path().display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        // sorted lexicographically
        assert_eq!(files[0]["name"], "a.txt");
        assert_eq!(files[1]["name"], "b.txt");
        assert_eq!(files[2]["name"], "sub");
        assert_eq!(files[2]["type"], "dir");
    }

    #[tokio::test]
    async fn list_files_defaults_path_to_dot_when_omitted() {
        // Just assert the call works and returns success; the contents
        // depend on CWD.
        let v = run_list_files(json!({})).await.unwrap();
        assert_eq!(v["status"], "success");
    }

    #[tokio::test]
    async fn list_files_errors_when_path_is_a_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "x").await.unwrap();
        let v = run_list_files(json!({"path": path.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["error"].as_str().unwrap().contains("not a directory"));
    }

    #[tokio::test]
    async fn write_file_creates_missing_parents_and_writes_content() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c.txt");
        let v = run_write_file(json!({
            "path": nested.display().to_string(),
            "content": "hello",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["bytes_written"], 5);
        let read = tokio_fs::read_to_string(&nested).await.unwrap();
        assert_eq!(read, "hello");
    }

    #[tokio::test]
    async fn edit_file_replaces_substring_returns_count() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "foo bar foo baz foo").await.unwrap();
        let v = run_edit_file(json!({
            "path": path.display().to_string(),
            "old_text": "foo",
            "new_text": "qux",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["replacements"], 3);
        let read = tokio_fs::read_to_string(&path).await.unwrap();
        assert_eq!(read, "qux bar qux baz qux");
    }

    #[tokio::test]
    async fn edit_file_errors_when_pattern_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "hello").await.unwrap();
        let v = run_edit_file(json!({
            "path": path.display().to_string(),
            "old_text": "absent",
            "new_text": "x",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["code"], "not_found");
    }

    #[tokio::test]
    async fn fs_tools_register_under_canonical_and_alias_keys() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("read_file").is_some());
        assert!(reg.lookup("fs.read_file").is_some());
        assert!(reg.lookup("fs_read_file").is_some());
        assert!(reg.lookup("write_file").is_some());
        assert!(reg.lookup("edit_file").is_some());
        assert!(reg.lookup("list_files").is_some());
    }

    #[tokio::test]
    async fn write_and_edit_are_marked_destructive_read_is_not() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(!reg.lookup("read_file").unwrap().destructive);
        assert!(!reg.lookup("list_files").unwrap().destructive);
        assert!(reg.lookup("write_file").unwrap().destructive);
        assert!(reg.lookup("edit_file").unwrap().destructive);
    }

    // ── verb-layer primitives ───────────────────────────────────────

    #[tokio::test]
    async fn create_file_writes_when_absent_and_creates_parents() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c.txt");
        let v = run_create_file(json!({
            "path": nested.display().to_string(),
            "content": "fresh",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["bytes_written"], 5);
        assert_eq!(tokio_fs::read_to_string(&nested).await.unwrap(), "fresh");
    }

    #[tokio::test]
    async fn create_file_refuses_to_clobber_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "original").await.unwrap();
        let v = run_create_file(json!({
            "path": path.display().to_string(),
            "content": "new",
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["code"], "already_exists");
        // Original content untouched.
        assert_eq!(tokio_fs::read_to_string(&path).await.unwrap(), "original");
    }

    #[tokio::test]
    async fn delete_file_removes_and_reports_not_found() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "x").await.unwrap();
        let v = run_delete_file(json!({"path": path.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert!(!tokio_fs::try_exists(&path).await.unwrap());
        // Second delete → not_found.
        let again = run_delete_file(json!({"path": path.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(again["status"], "error");
        assert_eq!(again["code"], "not_found");
    }

    #[tokio::test]
    async fn delete_file_refuses_directory() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        tokio_fs::create_dir(&sub).await.unwrap();
        let v = run_delete_file(json!({"path": sub.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["error"].as_str().unwrap().contains("directory"));
    }

    #[tokio::test]
    async fn make_dir_creates_nested_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("x/y/z");
        let v = run_make_dir(json!({"path": nested.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert!(tokio_fs::metadata(&nested).await.unwrap().is_dir());
        // Idempotent — second call still succeeds.
        let again = run_make_dir(json!({"path": nested.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(again["status"], "success");
    }

    #[tokio::test]
    async fn remove_dir_removes_empty_but_refuses_nonempty_without_recursive() {
        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty");
        tokio_fs::create_dir(&empty).await.unwrap();
        let v = run_remove_dir(json!({"path": empty.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert!(!tokio_fs::try_exists(&empty).await.unwrap());

        let full = dir.path().join("full");
        tokio_fs::create_dir(&full).await.unwrap();
        tokio_fs::write(full.join("f.txt"), "x").await.unwrap();
        let blocked = run_remove_dir(json!({"path": full.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(blocked["status"], "error");
        assert!(tokio_fs::try_exists(&full).await.unwrap());
    }

    #[tokio::test]
    async fn remove_dir_recursive_removes_nonempty() {
        let dir = tempdir().unwrap();
        let full = dir.path().join("full");
        tokio_fs::create_dir(&full).await.unwrap();
        tokio_fs::write(full.join("f.txt"), "x").await.unwrap();
        let v = run_remove_dir(json!({
            "path": full.display().to_string(),
            "recursive": true,
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["recursive"], true);
        assert!(!tokio_fs::try_exists(&full).await.unwrap());
    }

    #[tokio::test]
    async fn remove_dir_not_found_and_not_a_dir() {
        let dir = tempdir().unwrap();
        let missing =
            run_remove_dir(json!({"path": dir.path().join("ghost").display().to_string()}))
                .await
                .unwrap();
        assert_eq!(missing["code"], "not_found");
        let file = dir.path().join("a.txt");
        tokio_fs::write(&file, "x").await.unwrap();
        let notdir = run_remove_dir(json!({"path": file.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(notdir["status"], "error");
        assert!(notdir["error"]
            .as_str()
            .unwrap()
            .contains("not a directory"));
    }
}
