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
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

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

/// Per-service pipe-name overrides, consulted by [`crate::pipe_name`].
///
/// Unlike [`BACKEND`] this is a process-global, not a thread-local, and the
/// difference is deliberate. `BACKEND` short-circuits before any transport, so
/// it is only ever read on the gpui test thread. An override here has to be
/// visible to the *real* transport, whose connect runs inside `call_inner` on
/// whatever tokio worker polls it — not necessarily the thread that set it. A
/// thread-local would silently fail to apply there. Each test binary is its own
/// process, so a global cannot leak across binaries; within one binary, use a
/// name unique per test (see `PipeNameOverride`).
static PIPE_NAME_OVERRIDES: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// Route `service` at `pipe` for the rest of the process, and restore the real
/// name on drop.
///
/// This is the seam that lets a test stand up a fixture server on a **private**
/// pipe while still exercising the real msgpack transport. Binding the
/// production name instead (`\\.\pipe\wylde-workspaces`) is what made
/// `integration_graph_ipc` fail with `ERROR_ACCESS_DENIED` on any machine
/// running the product — the live service already owns that name, and
/// `first_pipe_instance(true)` is refused. CI never runs the stack, so the name
/// was always free there and the gate could not see the failure (#75; same
/// shape as #47's prebuild guard).
///
/// Prefer a name unique to the test (`unique_pipe_name`) so two tests in one
/// binary can never collide with each other either.
#[must_use = "the override is reverted when the guard drops"]
pub struct PipeNameOverride {
    service: String,
    previous: Option<String>,
}

impl PipeNameOverride {
    /// Install an override routing `service` to the full pipe path `pipe`.
    pub fn install(service: &str, pipe: &str) -> Self {
        let service = bare_service(service);
        let previous = PIPE_NAME_OVERRIDES
            .write()
            .expect("pipe-name override lock poisoned")
            .get_or_insert_with(HashMap::new)
            .insert(service.clone(), pipe.to_owned());
        Self { service, previous }
    }
}

impl Drop for PipeNameOverride {
    fn drop(&mut self) {
        let mut guard = PIPE_NAME_OVERRIDES
            .write()
            .expect("pipe-name override lock poisoned");
        let map = guard.get_or_insert_with(HashMap::new);
        match self.previous.take() {
            Some(prev) => map.insert(self.service.clone(), prev),
            None => map.remove(&self.service),
        };
    }
}

/// A process-unique fixture pipe path for `service`, e.g.
/// `\\.\pipe\wylde-workspaces-test-31415`. Keyed on the pid so a crashed prior
/// run can never leave a bound name that fails the next one — the same
/// isolated-pipe precedent `rust/tests/parity/tests/lifecycle.rs` follows.
pub fn unique_pipe_name(service: &str) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        r"\\.\pipe\wylde-{}-test-{}-{}",
        bare_service(service),
        std::process::id(),
        n
    )
}

/// Normalise `wylde-workspaces` / `workspaces` to the bare key both
/// [`crate::pipe_name`] and the override map agree on.
fn bare_service(service: &str) -> String {
    service.strip_prefix("wylde-").unwrap_or(service).to_owned()
}

/// The override for `bare_service`, if a test installed one. Read by
/// [`crate::pipe_name`].
pub(crate) fn pipe_name_override(bare: &str) -> Option<String> {
    PIPE_NAME_OVERRIDES
        .read()
        .expect("pipe-name override lock poisoned")
        .as_ref()?
        .get(bare)
        .cloned()
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
