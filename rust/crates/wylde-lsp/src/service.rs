//! `lsp.*` verb surface — the pipe face of the rust-analyzer host (IDE S8).
//!
//! Every verb degrades cleanly: when rust-analyzer isn't installed/started the
//! actor returns an `unavailable` error and these handlers surface it as a
//! `lsp_unavailable` reply the editor treats as "no LSP, stay plain text".

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;
use wylde_shared::ipc::{register_action_with_meta, unregister_action, Reply};

use crate::client::{actor, path_to_uri, LspCommand};
use crate::config::Config;

const META_MODULE: &str = "wylde_lsp::service";

pub const STATUS: &str = "lsp.status";
pub const OPEN: &str = "lsp.open";
pub const CHANGE: &str = "lsp.change";
pub const COMPLETION: &str = "lsp.completion";
pub const HOVER: &str = "lsp.hover";
pub const DIAGNOSTICS: &str = "lsp.diagnostics";

pub const ALL_ACTIONS: &[&str] = &[STATUS, OPEN, CHANGE, COMPLETION, HOVER, DIAGNOSTICS];

static INSTALLED: AtomicBool = AtomicBool::new(false);

pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    register_action_with_meta(
        STATUS,
        |_p: Value| async move { handle_status().await },
        "rust-analyzer liveness. No payload. Reply: {server, running, available, \
         unavailable_reason}. `available:false` → the editor uses tree-sitter only.",
        META_MODULE,
    );
    register_action_with_meta(
        OPEN,
        |p: Value| async move { handle_open(p).await },
        "Open a document (lazily starts + initializes rust-analyzer against \
         `root`). Payload: {root, path, text, language?=rust}. Reply: {ok}. \
         lsp_unavailable when rust-analyzer can't run.",
        META_MODULE,
    );
    register_action_with_meta(
        CHANGE,
        |p: Value| async move { handle_change(p).await },
        "Full-text document change. Payload: {path, text, version?}. Reply: {ok}.",
        META_MODULE,
    );
    register_action_with_meta(
        COMPLETION,
        |p: Value| async move { handle_completion(p).await },
        "Completions at a position. Payload: {path, line, character} (0-based). \
         Reply: {items:[{label, detail?, kind?}]}.",
        META_MODULE,
    );
    register_action_with_meta(
        HOVER,
        |p: Value| async move { handle_hover(p).await },
        "Hover info at a position. Payload: {path, line, character} (0-based). \
         Reply: {contents} (plain text; empty when nothing to show).",
        META_MODULE,
    );
    register_action_with_meta(
        DIAGNOSTICS,
        |p: Value| async move { handle_diagnostics(p).await },
        "Latest cached diagnostics for a document (from publishDiagnostics). \
         Payload: {path}. Reply: {diagnostics:[{range, severity, message}]}.",
        META_MODULE,
    );
    tracing::info!("wylde-lsp: registered {} action(s)", ALL_ACTIONS.len());
}

pub fn reset_for_tests() {
    for n in ALL_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

// ── helpers ──────────────────────────────────────────────────────────────

fn require_str(p: &Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn position(p: &Value) -> Option<(u64, u64)> {
    Some((
        p.get("line").and_then(Value::as_u64)?,
        p.get("character").and_then(Value::as_u64)?,
    ))
}

/// Send a command to the actor and await its oneshot reply under the
/// configured request timeout.
async fn ask<T: Send + 'static>(
    make: impl FnOnce(oneshot::Sender<T>) -> LspCommand,
) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    actor()
        .send(make(tx))
        .map_err(|_| "lsp actor unavailable".to_owned())?;
    let timeout = Duration::from_millis(Config::get().request_timeout_ms);
    tokio::time::timeout(timeout, rx)
        .await
        .map_err(|_| "lsp request timed out".to_owned())?
        .map_err(|_| "lsp actor dropped the reply".to_owned())
}

/// Map an `unavailable`/transport error string onto the stable wire code.
fn err_reply(e: String) -> Reply {
    Reply::err_msg("lsp_unavailable", e)
}

// ── handlers ─────────────────────────────────────────────────────────────

async fn handle_status() -> Reply {
    match ask(LspCommand::Status).await {
        Ok(v) => Reply::ok(v),
        Err(e) => err_reply(e),
    }
}

async fn handle_open(p: Value) -> Reply {
    let (Some(root), Some(path)) = (require_str(&p, "root"), require_str(&p, "path")) else {
        return Reply::err_msg("bad_request", "root and path are required");
    };
    let text = p.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
    let language_id = require_str(&p, "language").unwrap_or_else(|| "rust".to_owned());
    let uri = path_to_uri(&path);
    let outcome: Result<(), String> = match ask(|reply| LspCommand::Open {
        root,
        uri,
        language_id,
        text,
        reply,
    })
    .await
    {
        Ok(inner) => inner,
        Err(e) => return err_reply(e),
    };
    match outcome {
        Ok(()) => Reply::ok(json!({ "ok": true })),
        Err(e) => err_reply(e),
    }
}

async fn handle_change(p: Value) -> Reply {
    let Some(path) = require_str(&p, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    let text = p.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
    let version = p.get("version").and_then(Value::as_i64).unwrap_or(2);
    let uri = path_to_uri(&path);
    let outcome: Result<(), String> = match ask(|reply| LspCommand::Change {
        uri,
        version,
        text,
        reply,
    })
    .await
    {
        Ok(inner) => inner,
        Err(e) => return err_reply(e),
    };
    match outcome {
        Ok(()) => Reply::ok(json!({ "ok": true })),
        Err(e) => err_reply(e),
    }
}

async fn handle_completion(p: Value) -> Reply {
    let Some(path) = require_str(&p, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    let Some((line, character)) = position(&p) else {
        return Reply::err_msg("bad_request", "line and character are required");
    };
    let params = json!({
        "textDocument": { "uri": path_to_uri(&path) },
        "position": { "line": line, "character": character },
    });
    match request("textDocument/completion", params).await {
        Ok(v) => Reply::ok(json!({ "items": parse_completion(&v) })),
        Err(e) => err_reply(e),
    }
}

async fn handle_hover(p: Value) -> Reply {
    let Some(path) = require_str(&p, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    let Some((line, character)) = position(&p) else {
        return Reply::err_msg("bad_request", "line and character are required");
    };
    let params = json!({
        "textDocument": { "uri": path_to_uri(&path) },
        "position": { "line": line, "character": character },
    });
    match request("textDocument/hover", params).await {
        Ok(v) => Reply::ok(json!({ "contents": parse_hover(&v) })),
        Err(e) => err_reply(e),
    }
}

async fn handle_diagnostics(p: Value) -> Reply {
    let Some(path) = require_str(&p, "path") else {
        return Reply::err_msg("bad_request", "path is required");
    };
    let uri = path_to_uri(&path);
    match ask(|reply| LspCommand::Diagnostics { uri, reply }).await {
        Ok(diags) => Reply::ok(json!({ "diagnostics": diags.iter().map(simplify_diagnostic).collect::<Vec<_>>() })),
        Err(e) => err_reply(e),
    }
}

/// Send a request-expecting-response and flatten the transport + LSP errors.
async fn request(method: &str, params: Value) -> Result<Value, String> {
    let method = method.to_owned();
    let inner: Result<Value, String> = ask(|reply| LspCommand::Request {
        method,
        params,
        reply,
    })
    .await?;
    inner
}

/// Flatten an LSP completion result (`CompletionItem[]` or `CompletionList`)
/// into `[{label, detail?, kind?}]`.
pub fn parse_completion(v: &Value) -> Vec<Value> {
    let items = if let Some(arr) = v.as_array() {
        arr.clone()
    } else if let Some(arr) = v.get("items").and_then(Value::as_array) {
        arr.clone()
    } else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            let label = it.get("label").and_then(Value::as_str)?;
            let mut o = json!({ "label": label });
            if let Some(d) = it.get("detail").and_then(Value::as_str) {
                o["detail"] = json!(d);
            }
            if let Some(k) = it.get("kind").and_then(Value::as_u64) {
                o["kind"] = json!(k);
            }
            Some(o)
        })
        .collect()
}

/// Flatten an LSP hover result's `contents` (string | MarkedString |
/// MarkupContent | array thereof) into a single plain string.
pub fn parse_hover(v: &Value) -> String {
    fn one(c: &Value) -> Option<String> {
        if let Some(s) = c.as_str() {
            return Some(s.to_owned());
        }
        // MarkupContent { kind, value } or MarkedString { language, value }.
        if let Some(s) = c.get("value").and_then(Value::as_str) {
            return Some(s.to_owned());
        }
        None
    }
    let Some(contents) = v.get("contents") else {
        return String::new();
    };
    if let Some(arr) = contents.as_array() {
        arr.iter()
            .filter_map(one)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        one(contents).unwrap_or_default()
    }
}

/// Reduce an LSP diagnostic to `{range, severity, message}`.
fn simplify_diagnostic(d: &Value) -> Value {
    json!({
        "range": d.get("range").cloned().unwrap_or(Value::Null),
        "severity": d.get("severity").and_then(Value::as_u64).unwrap_or(1),
        "message": d.get("message").and_then(Value::as_str).unwrap_or(""),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_completion_handles_both_shapes() {
        let list = json!({ "items": [
            { "label": "push", "detail": "fn push(...)", "kind": 2 },
            { "label": "pop" },
        ]});
        let out = parse_completion(&list);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["label"], "push");
        assert_eq!(out[0]["detail"], "fn push(...)");
        assert_eq!(out[1]["label"], "pop");
        assert!(out[1].get("detail").is_none());

        let arr = json!([ { "label": "x" } ]);
        assert_eq!(parse_completion(&arr).len(), 1);
        assert!(parse_completion(&json!(null)).is_empty());
    }

    #[test]
    fn parse_hover_flattens_variants() {
        assert_eq!(parse_hover(&json!({ "contents": "hello" })), "hello");
        assert_eq!(
            parse_hover(&json!({ "contents": { "kind": "markdown", "value": "**x**" } })),
            "**x**"
        );
        assert_eq!(
            parse_hover(&json!({ "contents": ["a", { "value": "b" }] })),
            "a\nb"
        );
        assert_eq!(parse_hover(&json!({})), "");
    }

    #[test]
    fn simplify_diagnostic_extracts_core_fields() {
        let d = json!({
            "range": { "start": {"line":1,"character":2}, "end": {"line":1,"character":5} },
            "severity": 2, "message": "unused variable", "code": "W0612"
        });
        let s = simplify_diagnostic(&d);
        assert_eq!(s["severity"], 2);
        assert_eq!(s["message"], "unused variable");
        assert!(s["range"].is_object());
    }
}
