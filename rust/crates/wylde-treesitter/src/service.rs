//! Service entrypoint: register the `treesitter.*` action surface on the
//! shared IPC registry. Same shape as `wylde-ollama::service`.
//!
//! Slice 1 registers exactly two verbs — `languages` and `parse`. The
//! chunk/extract_entities/outline/highlight verbs land in later slices.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{register_action_with_meta, unregister_action, IpcError, Reply};

use crate::parser;

const ALL_ACTIONS: [&str; 2] = ["treesitter.languages", "treesitter.parse"];

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

    tracing::info!("wylde-treesitter: registered {} actions", ALL_ACTIONS.len());
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
    use wylde_shared::ipc::{dispatch_action, list_actions};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_both_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let actions = list_actions();
        for n in ALL_ACTIONS {
            assert!(actions.contains(&n.to_string()), "missing {n}");
        }
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
