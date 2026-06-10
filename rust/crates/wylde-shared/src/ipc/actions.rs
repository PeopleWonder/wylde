//! Action-based dispatch — pipe-only handlers bypassing route tables.
//!
//! Rust port of `Core/shared/ipc/_actions.py`. Surfaces that must remain
//! unreachable over HTTP (orchestrator harness, model-state cache, egress
//! gateway) register handlers here; the pipe server dispatches them via
//! the literal method `/__action__` with `data = {"action", "payload"}`.
//!
//! The registry is also responsible for writing
//! `data/contracts/actions/<service>.json` at server-start time, so
//! `wylde_check` rules can read the same contract regardless of whether
//! the producing service is Python or Rust.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::ipc::wire::{IpcError, Reply};

/// Sentinel method path the pipe server routes to [`dispatch_action`].
pub const ACTION_DISPATCH_PATH: &str = "/__action__";

/// Erased async handler. Takes the payload, returns a [`Reply`].
pub type ActionHandler =
    Box<dyn Fn(serde_json::Value) -> Pin<Box<dyn Future<Output = Reply> + Send>> + Send + Sync>;

/// Sender side of a streaming handler's chunk sink. Send `Ok(payload)` per
/// chunk; sending `Err(IpcError)` surfaces a stream-level error to the
/// client and terminates the stream. Dropping the sender (or returning from
/// the handler) signals graceful end-of-stream — the server emits a final
/// `done=true` frame on the handler's behalf.
pub type StreamSender = tokio::sync::mpsc::Sender<Result<serde_json::Value, IpcError>>;

/// Erased async streaming handler. Takes the payload + a chunk sink.
pub type StreamingHandler = Box<
    dyn Fn(serde_json::Value, StreamSender) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Per-action metadata snapshotted into the contract file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionMeta {
    pub doc: String,
    pub handler_module: String,
    /// Was the action registered as a streaming handler? Plain (unary)
    /// handlers are `false`. Defaults to `false` on deserialize so older
    /// contract files still parse.
    #[serde(default)]
    pub streaming: bool,
}

struct Registry {
    handlers: HashMap<String, ActionHandler>,
    streaming: HashMap<String, StreamingHandler>,
    meta: HashMap<String, ActionMeta>,
}

impl Registry {
    fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            streaming: HashMap::new(),
            meta: HashMap::new(),
        }
    }
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Registry::new()))
}

/// Register an async action handler under `name`.
///
/// `handler` receives the payload (the value of `data.payload` in the
/// envelope) and returns a [`Reply`]. Re-registering an action replaces
/// the previous handler, matching the Python semantics.
pub fn register_action<F, Fut>(name: &str, handler: F)
where
    F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Reply> + Send + 'static,
{
    register_action_with_meta(name, handler, "", "")
}

/// Like [`register_action`] but lets the caller stamp the contract metadata
/// (doc first-line, handler module path). Rust services typically pass the
/// module path so the contract file matches what Python produces.
pub fn register_action_with_meta<F, Fut>(name: &str, handler: F, doc: &str, module: &str)
where
    F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Reply> + Send + 'static,
{
    assert!(!name.is_empty(), "action name must be non-empty");
    let boxed: ActionHandler = Box::new(move |payload| {
        let fut = handler(payload);
        Box::pin(fut) as Pin<Box<dyn Future<Output = Reply> + Send>>
    });
    let mut reg = registry().lock().expect("action registry poisoned");
    reg.handlers.insert(name.to_string(), boxed);
    reg.streaming.remove(name);
    reg.meta.insert(
        name.to_string(),
        ActionMeta {
            doc: doc.to_string(),
            handler_module: module.to_string(),
            streaming: false,
        },
    );
}

/// Register an async STREAMING action handler under `name`.
///
/// The handler receives the payload plus a [`StreamSender`]. Send
/// `Ok(payload)` to emit a chunk; send `Err(IpcError)` for a
/// stream-level error (the server forwards it and terminates the
/// stream). Returning (or dropping the sender) ends the stream
/// gracefully — the server emits a final `done=true` frame for you.
///
/// To react to client cancellation, `select!` on `sender.closed().await`
/// inside the handler; that future resolves the moment the server-side
/// reader is dropped (which happens when the client disconnects).
///
/// **Footgun:** in `async move` handlers, captures are inferred from
/// the body. If you bind the sender as `_sender` and never reference it
/// in the body, it is dropped before the future is first polled and the
/// stream ends immediately with no chunks. Always use the sender (or
/// bind it without a leading underscore and explicitly `drop(sender)`
/// at end-of-handler) so it lives for the duration of the handler.
pub fn register_streaming_action<F, Fut>(name: &str, handler: F)
where
    F: Fn(serde_json::Value, StreamSender) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    register_streaming_action_with_meta(name, handler, "", "")
}

/// Like [`register_streaming_action`] but with contract metadata, parallel
/// to [`register_action_with_meta`].
pub fn register_streaming_action_with_meta<F, Fut>(name: &str, handler: F, doc: &str, module: &str)
where
    F: Fn(serde_json::Value, StreamSender) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    assert!(!name.is_empty(), "action name must be non-empty");
    let boxed: StreamingHandler = Box::new(move |payload, sender| {
        let fut = handler(payload, sender);
        Box::pin(fut) as Pin<Box<dyn Future<Output = ()> + Send>>
    });
    let mut reg = registry().lock().expect("action registry poisoned");
    reg.streaming.insert(name.to_string(), boxed);
    reg.handlers.remove(name);
    reg.meta.insert(
        name.to_string(),
        ActionMeta {
            doc: doc.to_string(),
            handler_module: module.to_string(),
            streaming: true,
        },
    );
}

/// Remove an action binding. Idempotent — no error if the name was unbound.
/// Clears both unary and streaming entries under the name.
pub fn unregister_action(name: &str) {
    let mut reg = registry().lock().expect("action registry poisoned");
    reg.handlers.remove(name);
    reg.streaming.remove(name);
    reg.meta.remove(name);
}

/// Invoke a registered streaming handler by name, returning the future
/// the server should drive while pumping chunks off the matching receiver.
///
/// The lock is held only long enough to clone the future the closure
/// produces — the handler itself runs unlocked, identical to the unary
/// [`dispatch_action`] path.
pub fn take_streaming_action(
    name: &str,
    payload: serde_json::Value,
    sender: StreamSender,
) -> Result<Pin<Box<dyn Future<Output = ()> + Send>>, IpcError> {
    let reg = registry().lock().expect("action registry poisoned");
    match reg.streaming.get(name) {
        Some(h) => Ok(h(payload, sender)),
        None => Err(IpcError::new(
            "no_action",
            format!("unknown streaming action {name:?}"),
        )),
    }
}

/// True if `name` is registered as a streaming action. Used by the pipe
/// server to choose the dispatch path.
pub fn is_streaming_action(name: &str) -> bool {
    let reg = registry().lock().expect("action registry poisoned");
    reg.streaming.contains_key(name)
}

/// Snapshot of registered action names, sorted. For diagnostics / contract dumps.
pub fn list_actions() -> Vec<String> {
    let reg = registry().lock().expect("action registry poisoned");
    let mut names: Vec<String> = reg.handlers.keys().cloned().collect();
    names.sort();
    names
}

/// Snapshot of action → metadata, sorted by name.
pub fn list_action_meta() -> Vec<(String, ActionMeta)> {
    let reg = registry().lock().expect("action registry poisoned");
    let mut entries: Vec<(String, ActionMeta)> = reg
        .meta
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Resolve the action named in `payload['action']` and invoke its handler.
///
/// The payload shape is the standard action envelope:
/// `{"action": "<name>", "payload": <handler-input>}`. Errors are returned
/// as a `Reply { ok: false, error: Some(...) }` matching Python's
/// `_dispatch_action` shape (codes: `bad_request`, `no_action`, `handler`).
pub async fn dispatch_action(payload: serde_json::Value) -> Reply {
    let obj = match payload.as_object() {
        Some(o) => o,
        None => {
            return Reply::err(IpcError::new("bad_request", "action payload must be a map"));
        }
    };
    let name = match obj.get("action").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Reply::err(IpcError::new("bad_request", "missing 'action' field"));
        }
    };
    let inner = obj
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // Resolve under the lock, then drop it before awaiting so the registry
    // stays unlocked for the duration of the handler.
    let fut = {
        let reg = registry().lock().expect("action registry poisoned");
        match reg.handlers.get(&name) {
            Some(h) => h(inner),
            None => {
                return Reply::err(IpcError::new(
                    "no_action",
                    format!("unknown action {name:?}"),
                ));
            }
        }
    };
    fut.await
}

/// Write `data/contracts/actions/<service>.json` from the current registry.
///
/// Mirrors Python's `_write_action_contract` byte-for-byte (same fields,
/// same sort order, atomic via tmpfile + rename). The contract is the
/// cross-language source of truth `wylde_check` reads, so the format must
/// not drift.
///
/// Failures are returned to the caller so the pipe server can log-and-swallow
/// in its own style (matching the Python "warning + continue" path).
pub fn write_action_contract(service: &str, wylde_root: &Path) -> std::io::Result<PathBuf> {
    let contracts_dir = wylde_root.join("data").join("contracts").join("actions");
    std::fs::create_dir_all(&contracts_dir)?;
    let contract_path = contracts_dir.join(format!("{service}.json"));

    let entries = list_action_meta();
    let actions: Vec<String> = entries.iter().map(|(k, _)| k.clone()).collect();
    let details: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .map(|(k, v)| {
            let mut m = serde_json::Map::new();
            m.insert("doc".into(), serde_json::Value::String(v.doc.clone()));
            m.insert(
                "handler_module".into(),
                serde_json::Value::String(v.handler_module.clone()),
            );
            (k.clone(), serde_json::Value::Object(m))
        })
        .collect();

    let payload = serde_json::json!({
        "service": service,
        "actions": actions,
        "details": serde_json::Value::Object(details),
        "written_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    });

    // sort_keys=true to match Python's json.dumps(..., sort_keys=True)
    let body = serde_json_sorted::to_string_pretty(&payload)?;

    let tmp = contract_path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, &contract_path)?;
    Ok(contract_path)
}

// Tiny helper module: serde_json has no built-in sort_keys for pretty output.
// We re-serialize through a BTreeMap-backed Value to get deterministic order.
mod serde_json_sorted {
    use std::collections::BTreeMap;

    pub fn to_string_pretty(v: &serde_json::Value) -> std::io::Result<String> {
        let sorted = sort(v);
        serde_json::to_string_pretty(&sorted).map_err(std::io::Error::other)
    }

    fn sort(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let btree: BTreeMap<&String, serde_json::Value> =
                    m.iter().map(|(k, v)| (k, sort(v))).collect();
                let mut out = serde_json::Map::new();
                for (k, v) in btree {
                    out.insert(k.clone(), v);
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests run sequentially through this guard so the shared global registry
    // doesn't have action names clobber each other across parallel test threads.
    fn reset_for_test(names: &[&str]) {
        for n in names {
            unregister_action(n);
        }
    }

    #[tokio::test]
    async fn register_dispatch_unregister() {
        reset_for_test(&["t.echo"]);
        register_action("t.echo", |payload: serde_json::Value| async move {
            Reply::ok(payload)
        });
        assert!(list_actions().contains(&"t.echo".to_string()));

        let reply = dispatch_action(serde_json::json!({
            "action": "t.echo",
            "payload": {"hi": 1},
        }))
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["hi"], 1);

        unregister_action("t.echo");
        let reply = dispatch_action(serde_json::json!({
            "action": "t.echo",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
    }

    #[tokio::test]
    async fn dispatch_rejects_non_map_payload() {
        let reply = dispatch_action(serde_json::json!("nope")).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn dispatch_rejects_missing_action_field() {
        let reply = dispatch_action(serde_json::json!({"payload": 1})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn handler_can_return_error_reply() {
        reset_for_test(&["t.boom"]);
        register_action("t.boom", |_p: serde_json::Value| async move {
            Reply::err_msg("custom", "nope")
        });
        let reply = dispatch_action(serde_json::json!({
            "action": "t.boom",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "custom");
        unregister_action("t.boom");
    }

    #[test]
    fn list_actions_is_sorted() {
        reset_for_test(&["t.b", "t.a"]);
        register_action("t.b", |_v: serde_json::Value| async move {
            Reply::ok(serde_json::Value::Null)
        });
        register_action("t.a", |_v: serde_json::Value| async move {
            Reply::ok(serde_json::Value::Null)
        });
        let mut acts = list_actions();
        // Filter to just our keys to be robust against parallel test contamination.
        acts.retain(|n| n == "t.a" || n == "t.b");
        assert_eq!(acts, vec!["t.a".to_string(), "t.b".to_string()]);
        unregister_action("t.a");
        unregister_action("t.b");
    }

    #[test]
    fn writes_contract_with_matching_fields() {
        reset_for_test(&["t.contract"]);
        register_action_with_meta(
            "t.contract",
            |_v: serde_json::Value| async move { Reply::ok(serde_json::Value::Null) },
            "test action",
            "test_module",
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_action_contract("test-svc", dir.path()).expect("write");
        assert!(path.ends_with("data/contracts/actions/test-svc.json"));
        let body = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse");
        assert_eq!(v["service"], "test-svc");
        let actions = v["actions"].as_array().expect("actions array");
        assert!(actions.iter().any(|x| x == "t.contract"));
        let details = &v["details"]["t.contract"];
        assert_eq!(details["doc"], "test action");
        assert_eq!(details["handler_module"], "test_module");
        unregister_action("t.contract");
    }
}
