//! Service entrypoint: register the `treesitter.*` action surface on the
//! shared IPC registry. Same shape as `wylde-ollama::service`.
//!
//! Slice 1 registered `languages` + `parse`. Slice 2 added `chunk`
//! (AST-boundary-aware chunking) and the HTTP front door (`http.rs`) that
//! shares these handlers. Slice 3 added `extract_entities` (structural entities
//! for the Memgraph graph layer). Slice 4 widened the grammar set to Python,
//! Rust, TypeScript, TSX, JavaScript, and Markdown (no new verbs — every verb
//! above now answers for all six; TSX adds JSX-aware parsing + JSX component
//! CALLS edges). TBS Slice H added the IDE verbs — `outline` (nested symbol
//! tree) and `highlight` (syntax spans via the grammars' bundled queries) —
//! completing the plan's six-verb API surface.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{register_action_with_meta, unregister_action, IpcError, Reply};

use crate::{chunk, entities, highlight, outline, parser};

const ALL_ACTIONS: [&str; 6] = [
    "treesitter.languages",
    "treesitter.parse",
    "treesitter.chunk",
    "treesitter.extract_entities",
    "treesitter.outline",
    "treesitter.highlight",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `treesitter.*` action on the process-wide registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    register_action_with_meta(
        "treesitter.languages",
        |_payload: Value| async move { Reply::ok(parser::languages()) },
        "{} — list statically-linked grammars. Reply: {languages:[{name, grammar_sha, abi}]}.",
        "wylde_treesitter::parser",
    );

    register_action_with_meta(
        "treesitter.parse",
        |payload: Value| async move { handle_parse(payload) },
        "{source, language} — parse inline source to a bounded AST sketch \
         (node kinds + ranges, no source bytes). Slice-1 escape hatch.",
        "wylde_treesitter::parser",
    );

    register_action_with_meta(
        "treesitter.chunk",
        |payload: Value| async move { handle_chunk(payload) },
        "{path, language?, max_chunk_bytes?} — AST-boundary-aware chunking. \
         Reply: {chunks:[{start_line,end_line,byte_start,byte_end,kind,symbol_name?}]}. \
         Splits on function/class boundaries; byte windows for unknown languages.",
        "wylde_treesitter::chunk",
    );

    register_action_with_meta(
        "treesitter.extract_entities",
        |payload: Value| async move { handle_extract_entities(payload) },
        "{path, language?} — structural entities for the graph layer. Reply: \
         {functions:[{name,line}], classes:[{name,line,methods,bases}], \
         imports:[{module,line}], calls:[{caller,callee,line}], module, counts}. \
         Feeds memgraph.upsert entities + relate CALLS/IMPORTS/INHERITS.",
        "wylde_treesitter::entities",
    );

    register_action_with_meta(
        "treesitter.outline",
        |payload: Value| async move { handle_outline(payload) },
        "{path, language?} — nested symbol outline (Slice H). Reply: \
         {tree:[{kind, name, line, end_line, children:[…]}]}. Definitions at \
         every depth, nested by containment (methods under their class).",
        "wylde_treesitter::outline",
    );

    register_action_with_meta(
        "treesitter.highlight",
        |payload: Value| async move { handle_highlight(payload) },
        "{path, language?} — syntax-highlight spans (Slice H). Reply: \
         {spans:[{start_byte, end_byte, scope}]}. Scopes are the grammar's \
         bundled highlights.scm capture names; consumers map scope → colour.",
        "wylde_treesitter::highlight",
    );

    tracing::info!("wylde-treesitter: registered {} actions", ALL_ACTIONS.len());
}

/// `treesitter.outline` handler — validate then delegate to
/// [`outline::outline`]. Shared by the pipe surface and the HTTP route.
pub fn handle_outline(payload: Value) -> Reply {
    let (path, language) = match path_and_language(&payload) {
        Ok(pl) => pl,
        Err(e) => return Reply::err(e),
    };
    match outline::outline(path, language) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(e),
    }
}

/// `treesitter.highlight` handler — validate then delegate to
/// [`highlight::highlight`]. Shared by the pipe surface and the HTTP route.
pub fn handle_highlight(payload: Value) -> Reply {
    let (path, language) = match path_and_language(&payload) {
        Ok(pl) => pl,
        Err(e) => return Reply::err(e),
    };
    // Optional inline `source` — the live-editor path (IDE S4): highlight the
    // caller's in-memory buffer instead of reading `path` from disk.
    let source = payload.get("source").and_then(Value::as_str);
    match highlight::highlight(path, language, source) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(e),
    }
}

/// The `{path, language?}` payload shape `outline`/`highlight` share.
fn path_and_language(payload: &Value) -> Result<(&str, Option<&str>), IpcError> {
    let path = match payload.get("path").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            return Err(IpcError::new(
                "invalid_request",
                "payload.path is required (non-empty string)",
            ))
        }
    };
    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    Ok((path, language))
}

/// `treesitter.chunk` handler — validate the payload then delegate to
/// [`chunk::chunk`]. Shared by the pipe action surface and the HTTP route
/// (`http.rs`) so the two transports can never drift.
pub fn handle_chunk(payload: Value) -> Reply {
    let path = match payload.get("path").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            return Reply::err(IpcError::new(
                "invalid_request",
                "payload.path is required (non-empty string)",
            ))
        }
    };
    // `language` is optional (inferred from the extension when omitted); an
    // empty string is treated as "omitted".
    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let max_chunk_bytes = payload
        .get("max_chunk_bytes")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    match chunk::chunk(path, language, max_chunk_bytes) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(e),
    }
}

/// `treesitter.extract_entities` handler — validate the payload then delegate
/// to [`entities::extract_entities`]. Shared by the pipe action surface and the
/// HTTP route (`http.rs`) so the two transports can never drift.
pub fn handle_extract_entities(payload: Value) -> Reply {
    let path = match payload.get("path").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            return Reply::err(IpcError::new(
                "invalid_request",
                "payload.path is required (non-empty string)",
            ))
        }
    };
    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());

    match entities::extract_entities(path, language) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(e),
    }
}

/// `treesitter.parse` handler — validate the payload then delegate to
/// [`parser::parse`].
fn handle_parse(payload: Value) -> Reply {
    let source = match payload.get("source").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            return Reply::err(IpcError::new(
                "invalid_request",
                "payload.source is required (string)",
            ))
        }
    };
    let language = match payload.get("language").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            return Reply::err(IpcError::new(
                "invalid_request",
                "payload.language is required (string)",
            ))
        }
    };

    match parser::parse(source, language) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(e),
    }
}

/// Signal stop. Currently a no-op — the service has no background workers
/// beyond the per-request handlers. Kept symmetric with the other services.
pub fn stop() {}

/// Test-only: unregister every action and reset the install flag.
pub fn reset_for_tests() {
    for n in ALL_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{assert_action_table_matches_registry, dispatch_action};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_all_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // #130: both directions — every registered treesitter.* verb must also
        // be listed in ALL_ACTIONS, not only the reverse.
        assert_action_table_matches_registry(&["treesitter."], &ALL_ACTIONS);
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        install();
        reset_for_tests();
    }

    #[tokio::test]
    async fn languages_dispatch_returns_python() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.languages",
            "payload": {},
        }))
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["languages"][0]["name"], "python");
        reset_for_tests();
    }

    #[tokio::test]
    async fn parse_dispatch_parses_python() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.parse",
            "payload": {"source": "x = 1\n", "language": "python"},
        }))
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["root"]["kind"], "module");
        reset_for_tests();
    }

    #[tokio::test]
    async fn chunk_dispatch_returns_ast_chunks() {
        use std::io::Write;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"def a():\n    return 1\n").unwrap();
        f.flush().unwrap();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.chunk",
            "payload": {"path": f.path().to_str().unwrap()},
        }))
        .await;
        assert!(reply.ok, "chunk dispatch failed: {:?}", reply.error);
        assert_eq!(reply.data["ast_aware"], true);
        assert_eq!(reply.data["chunks"][0]["symbol_name"], "a");
        reset_for_tests();
    }

    #[tokio::test]
    async fn extract_entities_dispatch_returns_structure() {
        use std::io::Write;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"import os\n\ndef a():\n    b()\n").unwrap();
        f.flush().unwrap();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.extract_entities",
            "payload": {"path": f.path().to_str().unwrap()},
        }))
        .await;
        assert!(
            reply.ok,
            "extract_entities dispatch failed: {:?}",
            reply.error
        );
        assert_eq!(reply.data["functions"][0]["name"], "a");
        assert_eq!(reply.data["imports"][0]["module"], "os");
        assert_eq!(reply.data["calls"][0]["callee"], "b");
        assert_eq!(reply.data["calls"][0]["caller"], "a");
        reset_for_tests();
    }

    #[tokio::test]
    async fn extract_entities_missing_path_is_invalid_request() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.extract_entities",
            "payload": {},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "invalid_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn chunk_missing_path_is_invalid_request() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.chunk",
            "payload": {},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "invalid_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn outline_dispatch_returns_a_nested_tree() {
        use std::io::Write;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"class C:\n    def m(self):\n        pass\n")
            .unwrap();
        f.flush().unwrap();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.outline",
            "payload": {"path": f.path().to_str().unwrap()},
        }))
        .await;
        assert!(reply.ok, "outline dispatch failed: {:?}", reply.error);
        assert_eq!(reply.data["tree"][0]["name"], "C");
        assert_eq!(reply.data["tree"][0]["children"][0]["name"], "m");
        reset_for_tests();
    }

    #[tokio::test]
    async fn highlight_dispatch_returns_scoped_spans() {
        use std::io::Write;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let mut f = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        f.write_all(b"def a():\n    return \"x\"\n").unwrap();
        f.flush().unwrap();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.highlight",
            "payload": {"path": f.path().to_str().unwrap()},
        }))
        .await;
        assert!(reply.ok, "highlight dispatch failed: {:?}", reply.error);
        assert!(reply.data["span_count"].as_u64().unwrap() > 0);
        reset_for_tests();
    }

    #[tokio::test]
    async fn outline_and_highlight_missing_path_are_invalid_request() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        for action in ["treesitter.outline", "treesitter.highlight"] {
            let reply = dispatch_action(serde_json::json!({
                "action": action,
                "payload": {},
            }))
            .await;
            assert!(!reply.ok, "{action}");
            assert_eq!(reply.error.unwrap().code, "invalid_request", "{action}");
        }
        reset_for_tests();
    }

    #[tokio::test]
    async fn parse_missing_source_is_invalid_request() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "treesitter.parse",
            "payload": {"language": "python"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "invalid_request");
        reset_for_tests();
    }
}
