//! The pipe calls the `/v1` handlers make, behind a trait so router tests
//! can run without live Wylde services.
//!
//! Production ([`PipeBackend`]) calls the harness model registry and
//! `wylde-ollama` over the named pipe. `wylde-ollama` owns VRAM leasing,
//! so every inference call here is brokered.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};
use wylde_shared::ipc::{call_action, IpcError};

/// Boxed future returned by [`Backend`] calls.
pub type BackendFuture = Pin<Box<dyn Future<Output = Result<Value, IpcError>> + Send>>;

/// Pipe service hosting the model registry (`models.list`).
pub const HARNESS_SERVICE: &str = "wylde-harness";
/// Pipe service hosting the `ollama.*` actions.
pub const OLLAMA_SERVICE: &str = "wylde-ollama";

pub trait Backend: Send + Sync {
    /// Harness `models.list` reply: `{models: [ModelEntry…], count, kind}`.
    fn registry_models(&self) -> BackendFuture;
    /// `ollama.list_models` reply (Ollama `/api/tags`): `{models: [{name, …}]}`.
    fn ollama_models(&self) -> BackendFuture;
    /// `ollama.embed` with `{model, input}`; reply is Ollama `/api/embed`.
    fn embed(&self, model: String, input: Value) -> BackendFuture;
}

/// The production backend: named-pipe calls to the live services.
pub struct PipeBackend;

impl Backend for PipeBackend {
    fn registry_models(&self) -> BackendFuture {
        Box::pin(call_action(HARNESS_SERVICE, "models.list", json!({})))
    }
    fn ollama_models(&self) -> BackendFuture {
        Box::pin(call_action(OLLAMA_SERVICE, "ollama.list_models", json!({})))
    }
    fn embed(&self, model: String, input: Value) -> BackendFuture {
        Box::pin(call_action(
            OLLAMA_SERVICE,
            "ollama.embed",
            json!({"model": model, "input": input}),
        ))
    }
}

/// The process-wide production backend.
pub fn pipe() -> Arc<dyn Backend> {
    Arc::new(PipeBackend)
}

/// Canned-reply test double.
#[cfg(test)]
pub mod testing {
    use std::sync::Mutex;

    use super::*;

    /// Each field is the reply the matching call returns.
    pub struct FakeBackend {
        pub registry: Result<Value, IpcError>,
        pub ollama: Result<Value, IpcError>,
        pub embed: Result<Value, IpcError>,
        /// `(model, input)` of every `embed` call.
        pub embed_calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeBackend {
        pub fn new(registry: Result<Value, IpcError>) -> Self {
            Self {
                registry,
                ollama: Err(IpcError::new("pipe_unavailable", "not faked")),
                embed: Err(IpcError::new("pipe_unavailable", "not faked")),
                embed_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Backend for FakeBackend {
        fn registry_models(&self) -> BackendFuture {
            let r = self.registry.clone();
            Box::pin(async move { r })
        }
        fn ollama_models(&self) -> BackendFuture {
            let r = self.ollama.clone();
            Box::pin(async move { r })
        }
        fn embed(&self, model: String, input: Value) -> BackendFuture {
            self.embed_calls.lock().unwrap().push((model, input));
            let r = self.embed.clone();
            Box::pin(async move { r })
        }
    }
}
