//! Pipe client for ``\\.\pipe\wylde-trainer-worker``.
//!
//! The Trainer service forwards every inference action to the Python
//! worker over its sibling pipe. We don't spawn the worker — the
//! lifecycle daemon does (``wylde-lifecycle::state::services::start_trainer_worker``).
//! The ``no_external_process_spawn_rust`` lint rule pins ``Command::new``
//! to the lifecycle crate; this module's only job is to forward
//! requests via the shared IPC client.

use serde_json::Value;
use wylde_shared::ipc::{send_action, IpcError, Reply};

/// Service name of the Python worker pipe — `\\.\pipe\wylde-trainer-worker`.
pub const WORKER_SERVICE: &str = "wylde-trainer-worker";

/// Forward `action` to the worker pipe with the given JSON payload.
///
/// Returns the worker's `Reply` envelope verbatim — the trainer's
/// action handler can either pass it back to its caller or post-process
/// the `data` field. We deliberately don't unwrap to `Result<Value,
/// IpcError>` here so callers can attach trainer-specific context
/// (action name, payload excerpt) to the error path.
pub async fn call_worker(action: &str, payload: Value) -> Reply {
    send_action(WORKER_SERVICE, action, payload).await
}

/// Convenience: map a worker `Reply` whose `data.error` field signals a
/// validation/handler failure (the Python tools' standard `{"error":
/// "..."}` envelope) to a proper `IpcError`. The Python tools return
/// `200`-shaped `Reply`s with the error embedded in `data`; this
/// flattens them so the trainer can surface a clean `worker_failed`
/// stable code instead of replying ok with an error-shaped body.
pub fn flatten_tool_error(reply: Reply) -> Reply {
    if !reply.ok {
        return reply;
    }
    let Some(msg) = reply
        .data
        .as_object()
        .and_then(|m| m.get("error"))
        .and_then(Value::as_str)
    else {
        return reply;
    };
    let msg = msg.to_owned();
    Reply::err(IpcError::new("worker_failed", msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_tool_error_passes_through_clean_reply() {
        let r = Reply::ok(json!({"caption": "a black dog"}));
        let out = flatten_tool_error(r);
        assert!(out.ok);
        assert_eq!(out.data["caption"], "a black dog");
    }

    #[test]
    fn flatten_tool_error_converts_embedded_error_envelope() {
        let r = Reply::ok(json!({"error": "image_path is required"}));
        let out = flatten_tool_error(r);
        assert!(!out.ok);
        let e = out.error.unwrap();
        assert_eq!(e.code, "worker_failed");
        assert_eq!(e.message, "image_path is required");
    }

    #[test]
    fn flatten_tool_error_preserves_existing_error_reply() {
        let r = Reply::err(IpcError::new("ipc_timeout", "took too long"));
        let out = flatten_tool_error(r);
        assert!(!out.ok);
        let e = out.error.unwrap();
        assert_eq!(e.code, "ipc_timeout");
    }
}
