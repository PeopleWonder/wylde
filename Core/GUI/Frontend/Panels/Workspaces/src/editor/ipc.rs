//! IPC helpers for the Editor tab (IDE S4).
//!
//! `fs.read` / `fs.write` go to the **jailed** `wylde-workspaces` file verbs
//! (S1) — the GUI never touches disk directly. `treesitter.highlight` goes to
//! the `wylde-treesitter` sidecar with the editor's **in-memory buffer** as an
//! inline `source` (S4 extension) so highlighting is live, not stale-on-disk.

use serde_json::{json, Value};

/// `workspaces.fs.read` — read a workspace-relative file (root-jailed).
/// Returns `{content, encoding, binary, truncated, size_bytes, mtime}`.
pub async fn read_file(workspace_id: &str, path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.fs.read",
            "payload": { "workspace_id": workspace_id, "path": path },
        })),
    )
    .await
}

/// `workspaces.fs.write` — atomically save text (root-jailed, optimistic
/// concurrency on `expected_mtime`). Returns `{mtime, size_bytes}`.
pub async fn write_file(
    workspace_id: &str,
    path: &str,
    content: &str,
    expected_mtime: Option<f64>,
) -> Result<Value, String> {
    let mut payload = json!({
        "workspace_id": workspace_id, "path": path, "content": content,
    });
    if let Some(m) = expected_mtime {
        payload["expected_mtime"] = json!(m);
    }
    wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({ "action": "workspaces.fs.write", "payload": payload })),
    )
    .await
}

// ── LSP (wylde-lsp service, IDE S9) ──────────────────────────────────────
//
// All best-effort: an `Err` (service down / rust-analyzer absent) means "no
// LSP", and the editor degrades to plain text + tree-sitter. `path` here is
// the file's ABSOLUTE path (rust-analyzer needs real file URIs + a root).

/// `lsp.open` — open a document (lazily starts rust-analyzer against `root`).
pub async fn lsp_open(root: &str, abs_path: &str, text: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-lsp",
        "POST",
        "/__action__",
        Some(json!({
            "action": "lsp.open",
            "payload": { "root": root, "path": abs_path, "text": text, "language": "rust" },
        })),
    )
    .await
}

/// `lsp.change` — full-text document change.
pub async fn lsp_change(abs_path: &str, text: &str, version: i64) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-lsp",
        "POST",
        "/__action__",
        Some(json!({
            "action": "lsp.change",
            "payload": { "path": abs_path, "text": text, "version": version },
        })),
    )
    .await
}

/// `lsp.diagnostics` — latest cached diagnostics for a document.
pub async fn lsp_diagnostics(abs_path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-lsp",
        "POST",
        "/__action__",
        Some(json!({ "action": "lsp.diagnostics", "payload": { "path": abs_path } })),
    )
    .await
}

/// `lsp.completion` — completions at a 0-based position.
pub async fn lsp_completion(abs_path: &str, line: u32, character: u32) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-lsp",
        "POST",
        "/__action__",
        Some(json!({
            "action": "lsp.completion",
            "payload": { "path": abs_path, "line": line, "character": character },
        })),
    )
    .await
}

/// `lsp.hover` — hover info at a 0-based position.
pub async fn lsp_hover(abs_path: &str, line: u32, character: u32) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-lsp",
        "POST",
        "/__action__",
        Some(json!({
            "action": "lsp.hover",
            "payload": { "path": abs_path, "line": line, "character": character },
        })),
    )
    .await
}

/// `treesitter.highlight` — syntax spans for the editor's live buffer.
/// `abs_path` supplies the file extension for grammar resolution; `source` is
/// the in-memory text (so highlighting reflects unsaved edits). Returns
/// `{spans:[{start_byte, end_byte, scope}]}`. An unsupported language /
/// unreachable sidecar surfaces as an `Err` the caller treats as "no
/// highlighting" (the editor still works in plain text).
pub async fn highlight(abs_path: &str, source: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-treesitter",
        "POST",
        "/__action__",
        Some(json!({
            "action": "treesitter.highlight",
            "payload": { "path": abs_path, "source": source },
        })),
    )
    .await
}
