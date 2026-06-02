//! Unified service-to-service transport layer.
//!
//! Rust port of the `Core/shared/ipc/` Python package. Discovery tells you
//! *where* a service is; this module tells you *how* to talk to it —
//! Windows named pipes are the canonical transport, HTTP loopback is the
//! fallback when the pipe stack isn't available.
//!
//! ## Usage at a call site
//!
//! ```no_run
//! # async fn demo() -> wylde_shared::ipc::Reply {
//! use wylde_shared::ipc;
//! ipc::call("tool-runner", "execute",
//!     serde_json::json!({"lang": "python", "code": "..."})).await
//! # }
//! ```
//!
//! ## Usage in a service's startup
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use wylde_shared::ipc;
//! ipc::serve("tool-runner", None).await
//! # }
//! ```
//!
//! ## Wire format
//!
//! `[u32 big-endian length][rmp-serde body]`, framed identically to what
//! Python's `_wire.py` produces. See [`wire`] for the exact encoders;
//! [`tests/fixtures/wire_corpus.json`](../../../tests/fixtures/wire_corpus.json)
//! pins the cross-language byte-level shape.
//!
//! Pipe naming convention: `\\.\pipe\wylde-<service-name>`.
//!
//! ## Streaming replies (additive, opt-in)
//!
//! On top of the unary request/reply path, the IPC layer supports
//! streaming replies for handlers that produce many chunks (LLM tokens,
//! progress events, log tails). Streaming is OPT-IN per handler — call
//! [`register_streaming_action`] instead of [`register_action`], and call
//! [`send_action_stream`] on the client side instead of [`send_action`].
//!
//! Wire shape for a streaming reply: MULTIPLE [`wire::ChunkFrame`] frames
//! on the same connection, each `[u32 BE length][rmp-serde body]`, all
//! sharing the same request-correlation `id`. `seq` starts at 0 and
//! increments per frame. `done=true` terminates the stream. A heartbeat
//! frame (`payload=null`, `done=false`) is emitted every
//! [`wire::STREAM_HEARTBEAT_SECS`] (25s) of handler silence so the
//! client's `WYLDE_IPC_IDLE_TIMEOUT` doesn't fire.
//!
//! ### Cancellation contract
//!
//! Dropping the client-side stream closes the underlying pipe handle.
//! The server detects the close on its next write (or via
//! [`tokio::sync::mpsc::Sender::is_closed`] on the chunk sender) and
//! drops the chunk receiver, which causes the handler's next
//! `sender.send(...)` to fail. The handler can also `await
//! sender.closed()` in a `select!` to react to cancellation
//! cooperatively. There is no "ghost generation" — once the client is
//! gone, the handler's sink dies and the handler observes that fact.

pub mod actions;
pub mod client;
pub mod http_routes;
pub mod observability;
pub mod server;
pub mod wire;

pub use self::actions::{
    dispatch_action, list_action_meta, list_actions, register_action, register_action_with_meta,
    register_streaming_action, register_streaming_action_with_meta, take_streaming_action,
    unregister_action, write_action_contract, ActionMeta, StreamSender, ACTION_DISPATCH_PATH,
};
pub use self::client::{
    call, call_action, register_handler, send, send_action, send_action_stream, send_with_verb,
};
pub use self::http_routes::{HttpHandler, HttpRequest, HttpResponse, HttpRouteTable};
pub use self::observability::{log_call, payload_size};
pub use self::server::{
    serve, serve_forever_background, serve_with_http_routes, supports_ipc, PipeServer,
};
pub use self::wire::{
    decode_frame, decode_frame_async, encode_frame, pipe_name, write_frame, write_frame_async,
    ChunkFrame, EnvConfig, IpcError, Reply, IPC_VERSION, STREAM_HEARTBEAT_SECS,
};
