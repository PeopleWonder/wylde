//! DEV-ONLY gpui-test helpers for the Wylde GUI.
//!
//! The whole GUI talks to its services through `wylde_gui_pipe::call` /
//! `stream_call`. This crate provides a [`ScriptedBackend`] that plugs into
//! that single chokepoint (via the Pipe crate's `test-support` feature) and
//! answers calls with canned data, so a windowed `#[gpui::test]` drives real
//! panel logic — enter/leave scope, turn-send, empty-state — with **no live
//! backend stack**.
//!
//! It also *records* every call, so a test can assert on exactly what a panel
//! sent (e.g. that a docked turn carried the entered `workspace_id`).
//!
//! ## Usage
//!
//! ```ignore
//! use wylde_gui_test_support::ScriptedBackend;
//! use serde_json::json;
//!
//! #[gpui::test]
//! fn docked_turn_carries_workspace_id(cx: &mut gpui::TestAppContext) {
//!     let fake = ScriptedBackend::new()
//!         .on("conversations.list", json!({ "conversations": [] }))
//!         .on("conversations.new", json!({ "id": "c-fresh" }))
//!         .on("chat.start_turn", json!({ "turn_id": "t1", "conversation_id": "c-fresh" }));
//!     let _guard = fake.clone().install();          // cleared on drop
//!
//!     let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
//!     window.update(cx, |p, _w, cx| p.apply_workspace_scope(Some("ws-a".into()), cx)).unwrap();
//!     cx.run_until_parked();
//!     window.update(cx, |p, _w, cx| p.send_user_message("hi".into(), cx)).unwrap();
//!     cx.run_until_parked();
//!
//!     let send = fake.last_call_for("chat.start_turn").unwrap();
//!     assert_eq!(send.workspace_id().as_deref(), Some("ws-a"));
//! }
//! ```
//!
//! See `Core/GUI/docs/gui-testing.md` for the full guide.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

pub use wylde_gui_pipe::test_backend::{clear, install, FakeBackend};

/// One unary call a panel made through `wylde_gui_pipe::call`, captured for
/// assertions.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    /// The target service (e.g. `"wylde-harness"`).
    pub service: String,
    /// HTTP verb (`"POST"` / `"GET"`).
    pub verb: String,
    /// Request path (e.g. `"/__action__"`).
    pub path: String,
    /// The harness action, pulled from `body["action"]` when present.
    pub action: Option<String>,
    /// The action payload, pulled from `body["payload"]` (or `Null`).
    pub payload: Value,
}

impl RecordedCall {
    /// Convenience: read a string field out of the recorded payload — e.g.
    /// `call.payload_str("workspace_id")`.
    pub fn payload_str(&self, key: &str) -> Option<String> {
        self.payload
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
    }

    /// Convenience for the most-asserted field: the turn's `workspace_id`.
    pub fn workspace_id(&self) -> Option<String> {
        self.payload_str("workspace_id")
    }
}

/// A scripted fake IPC backend: routes unary calls by harness action to
/// canned JSON, serves canned stream chunk lists, and records every call.
///
/// Build one with [`ScriptedBackend::new`] + the `on*` setters, then
/// [`install`](ScriptedBackend::install) it for the duration of a test. The
/// returned [`BackendGuard`] clears the thread-local on drop, so tests stay
/// isolated even under `cargo test`'s default parallelism.
#[derive(Default)]
pub struct ScriptedBackend {
    unary: Mutex<HashMap<String, Value>>,
    errors: Mutex<HashMap<String, String>>,
    path_unary: Mutex<HashMap<String, Value>>,
    path_errors: Mutex<HashMap<String, String>>,
    streams: Mutex<HashMap<String, Vec<Value>>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl ScriptedBackend {
    /// A backend with no scripted responses. Unscripted unary actions return
    /// `Ok({})` (a soft default most panel readers tolerate); script the ones
    /// a test asserts on.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Script a unary `action` to return `data` (the `ok` payload). Chainable.
    pub fn on(self: Arc<Self>, action: impl Into<String>, data: Value) -> Arc<Self> {
        self.unary.lock().unwrap().insert(action.into(), data);
        self
    }

    /// Script a unary `action` to fail with `code: message`-style `err`.
    pub fn on_err(self: Arc<Self>, action: impl Into<String>, err: impl Into<String>) -> Arc<Self> {
        self.errors.lock().unwrap().insert(action.into(), err.into());
        self
    }

    /// Script a call routed by request **path** to return `data`. Chainable.
    ///
    /// Some panels (e.g. RemoteAccess over `wylde-vpn`) don't use the
    /// `/__action__` + `"action"` envelope — they issue HTTP-style
    /// `call(service, "GET", "/api/link/status", None)`. Those calls carry no
    /// `action`, so [`on`](Self::on) can't reach them; key on the path instead.
    pub fn on_path(self: Arc<Self>, path: impl Into<String>, data: Value) -> Arc<Self> {
        self.path_unary.lock().unwrap().insert(path.into(), data);
        self
    }

    /// Script a path-routed call to fail with `code: message`-style `err` —
    /// the path counterpart of [`on_err`](Self::on_err), for the
    /// no-action-envelope panels (see [`on_path`](Self::on_path)).
    pub fn on_path_err(
        self: Arc<Self>,
        path: impl Into<String>,
        err: impl Into<String>,
    ) -> Arc<Self> {
        self.path_errors.lock().unwrap().insert(path.into(), err.into());
        self
    }

    /// Script a streaming `action` (e.g. `"chat.stream_turn"`) to replay
    /// `chunks` (each an inner payload `Value`), then end.
    pub fn on_stream(self: Arc<Self>, action: impl Into<String>, chunks: Vec<Value>) -> Arc<Self> {
        self.streams.lock().unwrap().insert(action.into(), chunks);
        self
    }

    /// Convenience: set the `conversations.list` response from a row list.
    pub fn conversations(self: Arc<Self>, rows: Vec<Value>) -> Arc<Self> {
        self.on("conversations.list", serde_json::json!({ "conversations": rows }))
    }

    /// Install this backend on the current thread for the life of the
    /// returned guard. The guard clears it on drop.
    pub fn install(self: Arc<Self>) -> BackendGuard {
        install(self as Arc<dyn FakeBackend>);
        BackendGuard { _private: () }
    }

    /// All recorded calls, in order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Recorded calls whose harness action equals `action`, in order.
    pub fn calls_for(&self, action: &str) -> Vec<RecordedCall> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.action.as_deref() == Some(action))
            .cloned()
            .collect()
    }

    /// The most recent recorded call for `action`, if any.
    pub fn last_call_for(&self, action: &str) -> Option<RecordedCall> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|c| c.action.as_deref() == Some(action))
            .cloned()
    }

    /// How many times `action` was called.
    pub fn count_for(&self, action: &str) -> usize {
        self.calls_for(action).len()
    }

    /// How many times a call was made to `path` (for the action-less,
    /// path-routed panels — see [`on_path`](Self::on_path)).
    pub fn count_for_path(&self, path: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.path == path)
            .count()
    }
}

impl FakeBackend for ScriptedBackend {
    fn call(
        &self,
        service: &str,
        http_verb: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let action = body
            .and_then(|b| b.get("action"))
            .and_then(|a| a.as_str())
            .map(|s| s.to_owned());
        let payload = body
            .and_then(|b| b.get("payload"))
            .cloned()
            .unwrap_or(Value::Null);

        self.calls.lock().unwrap().push(RecordedCall {
            service: service.to_owned(),
            verb: http_verb.to_owned(),
            path: path.to_owned(),
            action: action.clone(),
            payload,
        });

        if let Some(action) = action.as_deref() {
            if let Some(err) = self.errors.lock().unwrap().get(action) {
                return Err(err.clone());
            }
            if let Some(data) = self.unary.lock().unwrap().get(action) {
                return Ok(data.clone());
            }
        }
        // No action match (or an action-less HTTP-style call): route by path.
        if let Some(err) = self.path_errors.lock().unwrap().get(path) {
            return Err(err.clone());
        }
        if let Some(data) = self.path_unary.lock().unwrap().get(path) {
            return Ok(data.clone());
        }
        // Unscripted: soft default. `{}` parses to "empty"/default for the
        // permissive `from_value` projections the panels use.
        Ok(Value::Object(Default::default()))
    }

    fn stream(
        &self,
        _service: &str,
        action: &str,
        _payload: &Value,
    ) -> Result<Vec<Result<Value, String>>, String> {
        let chunks = self
            .streams
            .lock()
            .unwrap()
            .get(action)
            .cloned()
            .unwrap_or_default();
        Ok(chunks.into_iter().map(Ok).collect())
    }
}

/// RAII handle from [`ScriptedBackend::install`]. Clears the thread-local
/// fake backend on drop so the next test starts clean.
#[must_use = "drop this guard at the END of the test; binding it to `_guard` keeps the fake installed"]
pub struct BackendGuard {
    _private: (),
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        clear();
    }
}
