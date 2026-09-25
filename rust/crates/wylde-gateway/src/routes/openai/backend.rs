//! The pipe calls the `/v1` handlers make, behind a trait so router tests
//! can run without live Wylde services.
//!
//! Production ([`PipeBackend`]) calls the harness model registry and
//! `wylde-ollama` over the named pipe. `wylde-ollama` owns VRAM leasing,
//! so every inference call here is brokered. Dropping a stream returned by
//! [`Backend::chat_stream`] / [`Backend::generate_stream`] closes the pipe,
//! which `wylde-ollama` treats as a client cancel.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use serde_json::{json, Value};
use wylde_shared::ipc::{call_action, send_action_stream, IpcError};

use crate::services::ollama::{harness_service, ollama_service};

/// Boxed future returned by unary [`Backend`] calls.
pub type BackendFuture = Pin<Box<dyn Future<Output = Result<Value, IpcError>> + Send>>;

/// Boxed frame stream returned by streaming [`Backend`] calls: one Ollama
/// NDJSON object per item.
pub type BackendStream = Pin<Box<dyn Stream<Item = Result<Value, IpcError>> + Send>>;

pub trait Backend: Send + Sync {
    /// Harness `models.list` reply: `{models: [ModelEntry…], count, kind}`.
    fn registry_models(&self) -> BackendFuture;
    /// `ollama.list_models` reply (Ollama `/api/tags`): `{models: [{name, …}]}`.
    fn ollama_models(&self) -> BackendFuture;
    /// `ollama.embed` with `{model, input}`; reply is Ollama `/api/embed`.
    fn embed(&self, model: String, input: Value) -> BackendFuture;
    /// `ollama.show` with `{model}`; reply is Ollama `/api/show`
    /// (its `capabilities` list says whether the model supports `insert`).
    fn show(&self, model: String) -> BackendFuture;
    /// `ollama.chat_stream` with an Ollama `/api/chat` payload. `/v1` uses the
    /// streaming actions even for non-streaming requests, so a client
    /// disconnect always cancels generation.
    fn chat_stream(&self, payload: Value) -> BackendStream;
    /// `ollama.generate_stream` with an Ollama `/api/generate` payload.
    fn generate_stream(&self, payload: Value) -> BackendStream;
}

/// The production backend: named-pipe calls to the live services (names
/// from [`crate::services::ollama`], overridable for integration tests).
pub struct PipeBackend;

/// One unary pipe call, owning its service name.
fn unary(service: String, action: &'static str, payload: Value) -> BackendFuture {
    Box::pin(async move { call_action(&service, action, payload).await })
}

impl Backend for PipeBackend {
    fn registry_models(&self) -> BackendFuture {
        unary(harness_service(), "models.list", json!({}))
    }
    fn ollama_models(&self) -> BackendFuture {
        unary(ollama_service(), "ollama.list_models", json!({}))
    }
    fn embed(&self, model: String, input: Value) -> BackendFuture {
        unary(
            ollama_service(),
            "ollama.embed",
            json!({"model": model, "input": input}),
        )
    }
    fn show(&self, model: String) -> BackendFuture {
        unary(ollama_service(), "ollama.show", json!({"model": model}))
    }
    fn chat_stream(&self, payload: Value) -> BackendStream {
        Box::pin(send_action_stream(
            &ollama_service(),
            "ollama.chat_stream",
            payload,
        ))
    }
    fn generate_stream(&self, payload: Value) -> BackendStream {
        Box::pin(send_action_stream(
            &ollama_service(),
            "ollama.generate_stream",
            payload,
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    /// Each field is what the matching call returns. Streaming calls yield
    /// `stream_frames` in order; with `hang_next_stream` set, the next
    /// stream yields nothing and stays pending until dropped.
    pub struct FakeBackend {
        pub registry: Result<Value, IpcError>,
        pub ollama: Result<Value, IpcError>,
        pub embed: Result<Value, IpcError>,
        pub show: Result<Value, IpcError>,
        pub stream_frames: Vec<Result<Value, IpcError>>,
        pub hang_next_stream: AtomicBool,
        /// `(model, input)` of every `embed` call.
        pub embed_calls: Mutex<Vec<(String, Value)>>,
        /// `(action, payload)` of every inference call.
        pub calls: Mutex<Vec<(&'static str, Value)>>,
        /// How many streams have been dropped (finished or cancelled).
        pub dropped_streams: Arc<AtomicUsize>,
    }

    impl FakeBackend {
        pub fn new(registry: Result<Value, IpcError>) -> Self {
            let unfaked = || Err(IpcError::new("pipe_unavailable", "not faked"));
            Self {
                registry,
                ollama: unfaked(),
                embed: unfaked(),
                show: Ok(json!({"capabilities": ["completion"]})),
                stream_frames: Vec::new(),
                hang_next_stream: AtomicBool::new(false),
                embed_calls: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                dropped_streams: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Payloads sent to `action`, in order.
        pub fn payloads(&self, action: &str) -> Vec<Value> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .filter(|(a, _)| *a == action)
                .map(|(_, p)| p.clone())
                .collect()
        }

        fn stream(&self, action: &'static str, payload: Value) -> BackendStream {
            self.calls.lock().unwrap().push((action, payload));
            let frames = self.stream_frames.clone();
            let hang = self.hang_next_stream.swap(false, Ordering::SeqCst);
            let guard = DropCounter(self.dropped_streams.clone());
            Box::pin(async_stream::stream! {
                let _guard = guard;
                if hang {
                    futures::future::pending::<()>().await;
                }
                for f in frames {
                    yield f;
                }
            })
        }
    }

    /// Counts a stream drop, whether it finished or was cancelled.
    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
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
        fn show(&self, _model: String) -> BackendFuture {
            let r = self.show.clone();
            Box::pin(async move { r })
        }
        fn chat_stream(&self, payload: Value) -> BackendStream {
            self.stream("chat_stream", payload)
        }
        fn generate_stream(&self, payload: Value) -> BackendStream {
            self.stream("generate_stream", payload)
        }
    }
}
