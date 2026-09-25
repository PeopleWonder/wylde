//! Action handler modules — one per logical group.
//!
//! * [`models`] — health, list_models, list_loaded, show, delete, eject.
//! * [`chat`] — chat (unary), chat_stream (streaming).
//! * [`generate`] — generate (unary), generate_stream (streaming); FIM/completions backend.
//! * [`admission`] — lease admission for generate, including the FIM never-displace rule.
//! * [`stream_relay`] — the NDJSON relay + evict-on-cancel policy shared by the streaming actions.
//! * [`embed`] — embed (unary).
//! * [`pull`] — pull (streaming, with retry-on-transient-error).
//!
//! All handlers map upstream Ollama responses to the wire shapes
//! documented in `docs/wylde-ollama-design.md §1a`. The stable error
//! codes are listed in design doc §1a; helpers live in this module's
//! [`error`] submodule so every handler reaches for the same names.

pub mod admission;
pub mod chat;
pub mod embed;
pub mod error;
pub mod gc;
pub mod generate;
pub mod models;
pub mod pull;
pub mod stream_relay;

pub use error::{invalid_request, ollama_http_err, ollama_unreachable_err};
