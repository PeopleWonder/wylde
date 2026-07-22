//! `workspaces.fs.*` verb handlers — the jailed file-I/O surface the IDE
//! editor and file-tree tabs depend on (S1 / plan P0.2).
//!
//! Three verbs, all root-jailed per workspace via [`super::jail`]:
//! - `workspaces.fs.read` — read one file's text (binary/oversized flagged).
//! - `workspaces.fs.write` — atomic save with optimistic-concurrency.
//! - `workspaces.fs.list_dir` — one directory level (lazy tree expansion).
//!
//! ## Watcher interaction (OQ-6)
//! `fs.write` deliberately does **not** call the indexer itself. It writes the
//! file (atomically) and lets the *existing* workspace file watcher observe the
//! change and schedule a debounced delta re-index — so the index/graph stay
//! fresh after an in-editor save without this path enqueuing anything. The
//! temp file used for the atomic rename is a dotfile, which the watcher's
//! `is_indexable_path` filter already ignores, so the only event the watcher
//! sees is the final rename to the real path: one save → one debounced index,
//! no feedback loop.

use std::io::Read;
use std::path::Path;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};

use crate::config::Config;
use crate::registry;

/// Pull a required non-empty string field, or `None`.
fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Resolve a workspace id to its folder, or `None` if unknown.
fn folder_for(workspace_id: &str) -> Option<String> {
    registry::get(workspace_id).map(|def| def.folder)
}

/// The uniform `not_found` reply for an unknown workspace id.
fn not_found_ws(workspace_id: &str) -> Reply {
    Reply::err_msg("not_found", format!("workspace {workspace_id:?} not found"))
}

/// File mtime as epoch seconds (`f64`), `0.0` if the platform can't report it.
/// Matches the indexer's `walk::mtime_secs` convention so editor save-conflict
/// checks compare against the same clock the index uses.
fn mtime_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `workspaces.fs.read` — read one workspace file's text.
///
/// Payload: `{ workspace_id, path }`. Reply:
/// `{ content, encoding, binary, truncated, size_bytes, mtime }`.
///
/// - `binary: true` (null byte in the first 1 KB) → `content` is empty and
///   `encoding: "binary"`; the editor refuses to open it (OQ-7).
/// - oversized (> `fs_max_read_bytes`, default 2 MB) → `truncated: true` and
///   only the first `fs_max_read_bytes` bytes are returned; the editor opens it
///   read-only with a visible banner (OQ-7). Never silently truncated.
/// - non-UTF-8 bytes → `encoding: "utf8-lossy"` (replacement chars).
pub async fn handle_read(payload: Value) -> Reply {
    let Some(workspace_id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(path) = require_string(&payload, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    let folder = match folder_for(&workspace_id) {
        Some(f) => f,
        None => return not_found_ws(&workspace_id),
    };
    let resolved = match super::jail::resolve(&folder, &path, true) {
        Ok(p) => p,
        Err(e) => return e.to_reply(),
    };

    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(e) => return Reply::err_msg("io", format!("stat {path:?}: {e}")),
    };
    if meta.is_dir() {
        return Reply::err_msg("bad_request", format!("is a directory: {path:?}"));
    }

    let size = meta.len();
    let max = Config::get().fs_max_read_bytes;
    let truncated = size > max;
    let read_len = size.min(max);

    let mut buf = Vec::with_capacity(read_len as usize);
    let file = match std::fs::File::open(&resolved) {
        Ok(f) => f,
        Err(e) => return Reply::err_msg("io", format!("open {path:?}: {e}")),
    };
    if let Err(e) = file.take(read_len).read_to_end(&mut buf) {
        return Reply::err_msg("io", format!("read {path:?}: {e}"));
    }

    // Binary sniff — a NUL in the first 1 KB is a strong non-text signal. Same
    // heuristic the indexer's walk uses, so the editor and the index agree.
    let binary = buf.iter().take(1024).any(|b| *b == 0);
    let (content, encoding) = if binary {
        (String::new(), "binary")
    } else {
        match String::from_utf8(buf) {
            Ok(s) => (s, "utf8"),
            Err(e) => (
                String::from_utf8_lossy(e.as_bytes()).into_owned(),
                "utf8-lossy",
            ),
        }
    };

    Reply::ok(json!({
        "content": content,
        "encoding": encoding,
        "binary": binary,
        "truncated": truncated,
        "size_bytes": size,
        "mtime": mtime_secs(&meta),
    }))
}

/// `workspaces.fs.write` — atomically save text to a workspace file.
///
/// Payload: `{ workspace_id, path, content, expected_mtime? }`. Reply:
/// `{ mtime, size_bytes }`.
///
/// - Atomic: writes a sibling dotfile temp then renames over the target, so a
///   reader never sees a half-written file.
/// - Optimistic concurrency: when `expected_mtime` is supplied and the file on
///   disk has a newer mtime, the write is refused with `conflict` (details
///   carry `current_mtime`) so the editor can prompt on an external edit.
pub async fn handle_write(payload: Value) -> Reply {
    let Some(workspace_id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(path) = require_string(&payload, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    let Some(content) = payload.get("content").and_then(Value::as_str) else {
        return Reply::err_msg("bad_request", "content is required");
    };
    let folder = match folder_for(&workspace_id) {
        Some(f) => f,
        None => return not_found_ws(&workspace_id),
    };
    let resolved = match super::jail::resolve(&folder, &path, false) {
        Ok(p) => p,
        Err(e) => return e.to_reply(),
    };

    // Optimistic-concurrency check against an external edit since the editor
    // last read the file.
    if let Some(expected) = payload.get("expected_mtime").and_then(Value::as_f64) {
        if let Ok(meta) = std::fs::metadata(&resolved) {
            let current = mtime_secs(&meta);
            // 1ms slack absorbs float round-trips through JSON.
            if current - expected > 0.001 {
                return Reply::err(IpcError {
                    code: "conflict".into(),
                    message: format!("file changed on disk since it was read: {path:?}"),
                    details: Some(json!({
                        "current_mtime": current,
                        "expected_mtime": expected,
                    })),
                });
            }
        }
    }

    if let Err(e) = atomic_write(&resolved, content.as_bytes()) {
        return Reply::err_msg("io", format!("write {path:?}: {e}"));
    }

    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(e) => return Reply::err_msg("io", format!("stat-after-write {path:?}: {e}")),
    };
    Reply::ok(json!({
        "mtime": mtime_secs(&meta),
        "size_bytes": meta.len(),
    }))
}

/// Write `bytes` to `target` atomically: a sibling dotfile temp + rename. The
/// dotfile name keeps the watcher's `is_indexable_path` filter from indexing
/// the transient; only the final rename to the real path is observed.
fn atomic_write(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
    })?;
    let leaf = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{leaf}.wylde-tmp-{}-{}",
        std::process::id(),
        nanos
    ));

    // Scope the handle so it's flushed+closed before the rename.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // Rename over the destination. On Windows `rename` replaces an existing
    // file (std maps to MoveFileEx with replace semantics on modern toolchains
    // via a remove-then-rename fallback below if the platform refuses).
    match std::fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(_) if target.exists() => {
            // Replace-existing fallback for platforms whose rename won't clobber.
            std::fs::remove_file(target)?;
            let r = std::fs::rename(&tmp, target);
            if r.is_err() {
                let _ = std::fs::remove_file(&tmp); // best-effort temp-file cleanup (wylde-check: discard-result-ok)
            }
            r
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // best-effort temp-file cleanup (wylde-check: discard-result-ok)
            Err(e)
        }
    }
}

/// `workspaces.fs.list_dir` — one directory level under a workspace.
///
/// Payload: `{ workspace_id, path? }` (`path` defaults to the workspace root).
/// Reply: `{ path, entries: [{ name, kind, size_bytes?, mtime?, ignored }] }`.
///
/// `kind` is `"file" | "dir" | "symlink"`. `ignored: true` marks entries the
/// indexer's walk would skip (`.git`, `target`, `node_modules`, dotfiles,
/// binary suffixes) — the tree can dim or hide them, but they are still listed
/// (OQ-7: the tree shows binaries/oversized). Entries are sorted dirs-first
/// then case-insensitively by name. Lazy per-directory (OQ-4): one level only.
pub async fn handle_list_dir(payload: Value) -> Reply {
    let Some(workspace_id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    // `path` is optional; absent/empty means the workspace root.
    let rel = payload
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let folder = match folder_for(&workspace_id) {
        Some(f) => f,
        None => return not_found_ws(&workspace_id),
    };
    let resolved = match super::jail::resolve(&folder, &rel, true) {
        Ok(p) => p,
        Err(e) => return e.to_reply(),
    };
    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(e) => return Reply::err_msg("io", format!("stat {rel:?}: {e}")),
    };
    if !meta.is_dir() {
        return Reply::err_msg("bad_request", format!("not a directory: {rel:?}"));
    }

    let read_dir = match std::fs::read_dir(&resolved) {
        Ok(r) => r,
        Err(e) => return Reply::err_msg("io", format!("read_dir {rel:?}: {e}")),
    };

    let mut entries: Vec<Value> = Vec::new();
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let kind = if ft.is_dir() {
            "dir"
        } else if ft.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let full = entry.path();
        // The indexer's filter answers "would the walk skip this?" uniformly
        // for files (suffix/hidden) and dirs (skip-names/hidden). Classify each
        // entry relative to the (canonical) directory being listed, not the
        // workspace folder string: the ancestry up to here is already
        // jail-validated, and the canonical root may carry path components
        // (e.g. a dot-prefixed temp dir, or a `\\?\` prefix) that would
        // otherwise be mis-read as "hidden" and wrongly ignore everything.
        let ignored = !crate::rag::indexer::walk::is_indexable_path(
            &resolved.to_string_lossy(),
            &full.to_string_lossy(),
        );
        let (size_bytes, mtime) = match entry.metadata() {
            Ok(m) => (
                if m.is_file() { Some(m.len()) } else { None },
                Some(mtime_secs(&m)),
            ),
            Err(_) => (None, None),
        };
        let mut obj = json!({ "name": name, "kind": kind, "ignored": ignored });
        if let Some(sz) = size_bytes {
            obj["size_bytes"] = json!(sz);
        }
        if let Some(mt) = mtime {
            obj["mtime"] = json!(mt);
        }
        entries.push(obj);
    }

    // Dirs first, then case-insensitive name order — a stable, IDE-like tree.
    entries.sort_by(|a, b| {
        let ad = a["kind"] == json!("dir");
        let bd = b["kind"] == json!("dir");
        bd.cmp(&ad).then_with(|| {
            let an = a["name"].as_str().unwrap_or("").to_lowercase();
            let bn = b["name"].as_str().unwrap_or("").to_lowercase();
            an.cmp(&bn)
        })
    });

    Reply::ok(json!({ "path": rel, "entries": entries }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use tempfile::tempdir;

    /// Register a workspace folder and return its id.
    async fn register_ws(folder: &Path) -> String {
        crate::api::handle_create(json!({ "folder": folder.to_string_lossy() }))
            .await
            .data["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn read_returns_utf8_text() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        std::fs::write(td.path().join("a.rs"), "fn main() {}\n").unwrap();
        let id = register_ws(td.path()).await;

        let r = handle_read(json!({ "workspace_id": id, "path": "a.rs" })).await;
        assert!(r.ok, "read failed: {:?}", r.error);
        assert_eq!(r.data["content"], "fn main() {}\n");
        assert_eq!(r.data["encoding"], "utf8");
        assert_eq!(r.data["binary"], false);
        assert_eq!(r.data["truncated"], false);
    }

    #[tokio::test]
    async fn read_flags_binary_with_empty_content() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        std::fs::write(td.path().join("blob"), [b'a', 0u8, b'b', 1u8]).unwrap();
        let id = register_ws(td.path()).await;

        let r = handle_read(json!({ "workspace_id": id, "path": "blob" })).await;
        assert!(r.ok);
        assert_eq!(r.data["binary"], true);
        assert_eq!(r.data["encoding"], "binary");
        assert_eq!(r.data["content"], "");
    }

    #[tokio::test]
    async fn read_jail_rejects_escape() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        std::fs::write(td.path().join("a.rs"), "x").unwrap();
        let id = register_ws(td.path()).await;

        let r = handle_read(json!({ "workspace_id": id, "path": "../escape" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "path_escape");
    }

    #[tokio::test]
    async fn write_then_read_round_trips_and_is_atomic() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let id = register_ws(td.path()).await;

        let w = handle_write(json!({
            "workspace_id": id, "path": "new/file.rs", "content": "hello"
        }))
        .await;
        // Parent "new/" doesn't exist yet — write should still fail cleanly
        // (we don't auto-mkdir); assert the error is io, not a panic.
        assert!(
            !w.ok,
            "writing into a missing dir should be a clean io error"
        );
        assert_eq!(w.error.unwrap().code, "io");

        // Now a top-level file whose parent (root) exists.
        let w2 = handle_write(json!({
            "workspace_id": id, "path": "file.rs", "content": "hello"
        }))
        .await;
        assert!(w2.ok, "write failed: {:?}", w2.error);
        let r = handle_read(json!({ "workspace_id": id, "path": "file.rs" })).await;
        assert_eq!(r.data["content"], "hello");
        // No temp files left behind in the root.
        let leftovers: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("wylde-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked");
    }

    #[tokio::test]
    async fn write_conflict_on_stale_mtime() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        std::fs::write(td.path().join("f.rs"), "v1").unwrap();
        let id = register_ws(td.path()).await;

        // Pretend the editor read the file at mtime 0 (long ago); the on-disk
        // file is newer → conflict.
        let w = handle_write(json!({
            "workspace_id": id, "path": "f.rs", "content": "v2", "expected_mtime": 0.0
        }))
        .await;
        assert!(!w.ok);
        assert_eq!(w.error.unwrap().code, "conflict");
        // The file is untouched.
        assert_eq!(
            std::fs::read_to_string(td.path().join("f.rs")).unwrap(),
            "v1"
        );
    }

    #[tokio::test]
    async fn list_dir_lists_one_level_with_ignored_flags() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("main.rs"), "fn main(){}").unwrap();
        std::fs::write(root.join("logo.png"), "x").unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("src").join("deep.rs"), "deep").unwrap();
        let id = register_ws(root).await;

        let r = handle_list_dir(json!({ "workspace_id": id })).await;
        assert!(r.ok, "list_dir failed: {:?}", r.error);
        let entries = r.data["entries"].as_array().unwrap();
        // One level only — "deep.rs" is NOT here.
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"src"));
        assert!(names.contains(&"target"));
        assert!(!names.contains(&"deep.rs"));
        // Dirs sort first.
        assert_eq!(entries[0]["kind"], "dir");
        // `target` is listed but flagged ignored; `main.rs` is not.
        let target = entries.iter().find(|e| e["name"] == "target").unwrap();
        assert_eq!(target["ignored"], true);
        let png = entries.iter().find(|e| e["name"] == "logo.png").unwrap();
        assert_eq!(png["ignored"], true, "binary suffix is ignored-flagged");
        let main = entries.iter().find(|e| e["name"] == "main.rs").unwrap();
        assert_eq!(main["ignored"], false);
    }

    #[tokio::test]
    async fn list_dir_unknown_workspace_is_not_found() {
        let _env = TestEnv::new();
        let r = handle_list_dir(json!({ "workspace_id": "ghost-000000" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
    }
}
