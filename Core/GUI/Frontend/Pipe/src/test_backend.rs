//! Dev-only injectable IPC backend (the test seam).
//!
//! Every panel funnels its service calls through [`crate::call`] and
//! [`crate::stream_call`] — that single chokepoint is where a windowed
//! gpui test swaps the live named-pipe transport for canned responses, so
//! a test never needs a running backend stack.
//!
//! This whole module is gated behind the `test-support` Cargo feature.  A
//! normal Shell build never enables that feature (it's pulled in only via
//! the panels' `dev-dependencies`), so none of this — nor the two hook
//! sites in [`crate::call`] / [`crate::stream_call`] — compiles into the
//! shipped binary.  See `Core/GUI/docs/gui-testing.md`.
//!
//! ## Isolation
//!
//! The installed backend lives in a **thread-local**, not a global.  In
//! gpui test mode the `TestDispatcher` polls every task — foreground and
//! background — on the thread that drives `run_until_parked`, which is the
//! test thread.  A thread-local therefore gives each `#[gpui::test]` its
//! own backend with zero cross-test contention, even under `cargo test`'s
//! default parallelism.  Production code (feature off) reads none of this.

use std::cell::RefCell;
use std::sync::Arc;

use serde_json::Value;

/// A fake IPC transport: answers unary [`crate::call`]s and streaming
/// [`crate::stream_call`]s with canned data.
///
/// Implementors are `Send + Sync` so the `call` future stays `Send` while
/// the `test-support` feature is on; the canned reply is produced
/// synchronously (no `.await`), so the gpui executor resolves it inline
/// during `run_until_parked` without a tokio runtime.
pub trait FakeBackend: Send + Sync {
    /// Answer a unary call.  `body` is the full request body the panel
    /// passed — for harness verbs that is `{"action": "...", "payload":
    /// {...}}`, so an implementor routes on `body["action"]`.
    fn call(
        &self,
        service: &str,
        http_verb: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String>;

    /// Answer a streaming call: return the chunk list to replay (each item
    /// an `Ok(payload)` or a transport-level `Err`).  The harness drains
    /// them in order, then ends the stream.  Defaults to an immediately
    /// empty stream.
    fn stream(
        &self,
        _service: &str,
        _action: &str,
        _payload: &Value,
    ) -> Result<Vec<Result<Value, String>>, String> {
        Ok(Vec::new())
    }
}

thread_local! {
    static BACKEND: RefCell<Option<Arc<dyn FakeBackend>>> = const { RefCell::new(None) };
}

/// Install `backend` as the current thread's fake transport.  Prefer the
/// RAII guard in `wylde-gui-test-support` so it's cleared on test exit.
pub fn install(backend: Arc<dyn FakeBackend>) {
    BACKEND.with(|b| *b.borrow_mut() = Some(backend));
}

/// Remove the current thread's fake transport (calls fall through to the
/// real named pipe again).
pub fn clear() {
    BACKEND.with(|b| *b.borrow_mut() = None);
}

/// The current thread's fake transport, if one is installed.  Read at the
/// top of [`crate::call`] / [`crate::stream_call`].
pub(crate) fn current() -> Option<Arc<dyn FakeBackend>> {
    BACKEND.with(|b| b.borrow().clone())
}
