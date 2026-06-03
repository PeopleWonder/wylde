//! Harness pipe client — the seam the tool handlers call the S2a verbs
//! through, and the seam unit tests mock.
//!
//! Python Study imported `Core.harness.*` and called RAG/chat in-process. A
//! Rust binary can't, so every harness capability is reached over the pipe
//! via [`wylde_shared::ipc::call_action`]. The [`HarnessClient`] trait is the
//! single abstraction over that call so [`crate::tools`] handlers can be unit
//! tested against a canned mock without a live `wylde-harness`.

use serde_json::Value;
use wylde_shared::ipc::{self, IpcError};

use crate::config::Config;

/// One IPC round-trip to a named action on the harness pipe.
///
/// `Ok(data)` carries the action's `data` payload on an `ok` reply; `Err`
/// carries the structured [`IpcError`] the server emitted (transport failure
/// or an action that returned `Reply::err`, e.g. `chat.complete`'s
/// `bad_request` / `chat_failed`). Note the `rag.*` verbs instead return
/// `Ok(data)` with an in-band `{"status": "error", ...}` envelope — the
/// caller inspects `status` for those.
#[allow(async_fn_in_trait)] // static-dispatch only; no `dyn HarnessClient`.
pub trait HarnessClient {
    async fn call(&self, action: &str, payload: Value) -> Result<Value, IpcError>;
}

/// Production client — forwards to the real harness pipe named in [`Config`].
pub struct PipeClient {
    service: String,
}

impl PipeClient {
    /// Build a client targeting the configured harness service
    /// (`WYLDE_STUDY_HARNESS`, default `wylde-harness`).
    pub fn from_config() -> Self {
        Self {
            service: Config::get().harness_service.clone(),
        }
    }
}

impl HarnessClient for PipeClient {
    async fn call(&self, action: &str, payload: Value) -> Result<Value, IpcError> {
        ipc::call_action(&self.service, action, payload).await
    }
}
