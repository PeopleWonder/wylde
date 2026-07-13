//! Grammar-constrained decoding plumbing (Aaron's 2026-07-13 decision:
//! "add constrained decoding where logical").
//!
//! ## Where "logical" is (the policy this module encodes)
//!
//! Constrain **machine-consumed structured output, never human-read
//! prose**:
//!
//! | surface | constrained? | why |
//! |---|---|---|
//! | PLAN / REPLAN (S3) | **yes**, [`plan_format`] gated on `constrained_plan` | eval-backed: default reasoner 93.3% → 100% schema-valid, no speed/quality cost |
//! | L2 surprise verdict (S4) | yes, once it exists | single yes/no — a tiny enum schema; should never freehand |
//! | REFLECT critique (S5) | only if its output becomes a structured lessons record | today's reflection cycles emit prompt-shaped free text through `memory::reflection::ReflectionChat` — constraining prose degrades it |
//! | chat composition / final answer | **never** | human-read prose |
//! | tool-call rounds | **never** | the native Ollama `tools` field already constrains its own path |
//! | `<think>` stream | **never** (and *cannot* be) | verified live 2026-07-13: Ollama's `format` constrains only `message.content`; `message.thinking` flows untouched (byte-identical think text at fixed seed, constrained vs not). A model that ruminates past `think_budget_tokens` still fails the call — that budget, not the grammar, is the guard. |
//!
//! ## Transport
//!
//! `wylde-ollama` is a deliberate pass-through ("every Ollama-known field
//! flows through without remapping" — `actions/chat.rs`), so a `format`
//! key placed on the `ollama.chat` / `ollama.chat_stream` IPC body reaches
//! the daemon unmodified. Nothing service-side to change.
//!
//! ## Fail-soft
//!
//! The dev rig's Ollama build never *rejects* a malformed schema — it
//! silently generates unconstrained (verified live: bogus schemas → HTTP
//! 200). The retry here exists for backends/versions that DO reject: an
//! `ollama_http` error on a format-carrying call is retried once without
//! `format`, degrading to freehand + the caller's parse-failure fallback
//! (plain ReAct), never a hard error. Transport-level failures
//! (`ollama_unreachable`, broker errors) are NOT retried bare — they would
//! fail identically and the existing error paths own them.

use serde_json::Value;
use wylde_shared::ipc::{self, IpcError};

use super::config::ReasoningConfig;

/// The `format` value for a PLAN/REPLAN call, or `None` when the
/// `constrained_plan` toggle is off. The schema is the canonical one in
/// [`wylde_reasoning_plan::plan_dag_format`] — field-for-field lockstep
/// with the serde types EXECUTE deserializes.
pub fn plan_format() -> Option<Value> {
    ReasoningConfig::current()
        .constrained_plan
        .then(wylde_reasoning_plan::plan_dag_format)
}

/// Attach a `format` schema to an `ollama.chat`-shaped body. No-op on a
/// non-object body (nothing sane to do; the call will fail validation
/// upstream anyway).
pub fn attach_format(body: &mut Value, format: &Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("format".to_owned(), format.clone());
    }
}

/// `true` when `err` looks like the backend rejecting the request body —
/// the only class worth retrying without `format`. (`ollama_http` is
/// wylde-ollama's non-2xx envelope; transport/broker errors keep their
/// own codes and are not retried here.)
fn is_backend_rejection(err: &IpcError) -> bool {
    err.code == "ollama_http"
}

/// One unary constrained chat call with the fail-soft retry, generic over
/// the transport so the retry logic is unit-testable without a pipe.
pub async fn chat_maybe_constrained_via<F, Fut>(
    call: F,
    mut body: Value,
    format: Option<&Value>,
) -> Result<Value, IpcError>
where
    F: Fn(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, IpcError>>,
{
    let Some(fmt) = format else {
        return call(body).await;
    };
    attach_format(&mut body, fmt);
    match call(body.clone()).await {
        Err(e) if is_backend_rejection(&e) => {
            tracing::warn!(
                "reasoning: backend rejected format-constrained call ({}: {}); retrying freehand",
                e.code,
                e.message
            );
            if let Some(obj) = body.as_object_mut() {
                obj.remove("format");
            }
            call(body).await
        }
        other => other,
    }
}

/// Production wrapper: one unary `ollama.chat` IPC hop with the optional
/// schema + fail-soft retry. This is the call S3's PLAN phase makes.
pub async fn ollama_chat_maybe_constrained(
    service: &str,
    body: Value,
    format: Option<&Value>,
) -> Result<Value, IpcError> {
    chat_maybe_constrained_via(
        |b| ipc::call_action(service, "ollama.chat", b),
        body,
        format,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn body() -> Value {
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": false})
    }

    #[test]
    fn attach_format_inserts_key() {
        let mut b = body();
        attach_format(&mut b, &json!({"type": "object"}));
        assert_eq!(b["format"], json!({"type": "object"}));
    }

    #[test]
    fn attach_format_noop_on_non_object() {
        let mut b = json!("not an object");
        attach_format(&mut b, &json!({"type": "object"}));
        assert_eq!(b, json!("not an object"));
    }

    #[tokio::test]
    async fn no_format_passes_body_through_untouched() {
        let calls = AtomicUsize::new(0);
        let out = chat_maybe_constrained_via(
            |b| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert!(b.get("format").is_none(), "no format key without a schema");
                    Ok(json!({"ok": true}))
                }
            },
            body(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(out, json!({"ok": true}));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn format_attached_on_constrained_call() {
        let fmt = json!({"type": "object", "required": ["goal"]});
        let out = chat_maybe_constrained_via(
            |b| {
                let fmt = fmt.clone();
                async move {
                    assert_eq!(b["format"], fmt, "schema must ride the body");
                    Ok(json!({"ok": true}))
                }
            },
            body(),
            Some(&fmt),
        )
        .await
        .unwrap();
        assert_eq!(out, json!({"ok": true}));
    }

    #[tokio::test]
    async fn backend_rejection_retries_once_without_format() {
        let calls = AtomicUsize::new(0);
        let fmt = json!({"type": "object"});
        let out = chat_maybe_constrained_via(
            |b| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        assert!(b.get("format").is_some(), "first attempt is constrained");
                        Err(IpcError::new("ollama_http", "400: unsupported format"))
                    } else {
                        assert!(b.get("format").is_none(), "retry must be freehand");
                        Ok(json!({"freehand": true}))
                    }
                }
            },
            body(),
            Some(&fmt),
        )
        .await
        .unwrap();
        assert_eq!(out, json!({"freehand": true}));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn transport_error_is_not_retried_bare() {
        let calls = AtomicUsize::new(0);
        let fmt = json!({"type": "object"});
        let err =
            chat_maybe_constrained_via(
                |_b| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Err::<Value, _>(IpcError::new("ollama_unreachable", "connect refused"))
                    }
                },
                body(),
                Some(&fmt),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "ollama_unreachable");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unreachable daemon fails identically bare — no retry"
        );
    }

    #[test]
    fn plan_format_follows_the_toggle() {
        // Default config (constrained_plan: true) → Some(schema). This test
        // reads the process-global config cache; it asserts on the DEFAULT
        // (no reasoning.json in the test env), which carries the toggle ON.
        let f = plan_format().expect("default constrained_plan=true yields a schema");
        assert_eq!(f["required"][0], "goal", "it's the PlanDag schema");
        assert!(
            f["properties"]["steps"]["items"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "expected"),
            "steps require the surprise key"
        );
    }
}
