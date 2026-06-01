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

async fn run_read_file(args: Value) -> Result<Value, IpcError> {
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

async fn run_list_files(args: Value) -> Result<Value, IpcError> {
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

async fn run_write_file(args: Value) -> Result<Value, IpcError> {
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

async fn run_edit_file(args: Value) -> Result<Value, IpcError> {
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
    let new_text = args
        .get("new_text")
        .and_then(Value::as_str)
        .unwrap_or("");
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
        assert_eq!(
            v["content"].as_str().unwrap().len(),
            CONTENT_CAP
        );
    }

    #[tokio::test]
    async fn list_files_lists_directory_contents_sorted() {
        let dir = tempdir().unwrap();
        tokio_fs::write(dir.path().join("b.txt"), "b").await.unwrap();
        tokio_fs::write(dir.path().join("a.txt"), "aa").await.unwrap();
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
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("not a directory"));
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

}
