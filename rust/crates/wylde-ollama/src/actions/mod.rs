//! Action handler modules — one per logical group.
//!
//! * [`models`] — health, list_models, list_loaded, show, delete, eject.
//! * [`chat`] — chat (unary), chat_stream (streaming).
//! * [`embed`] — embed (unary).
//! * [`pull`] — pull (streaming, with retry-on-transient-error).
//!
//! All handlers map upstream Ollama responses to the wire shapes
//! documented in `docs/wylde-ollama-design.md §1a`. The stable error
//! codes are listed in design doc §1a; helpers live in this module's
//! [`error`] submodule so every handler reaches for the same names.

pub mod chat;
pub mod embed;
pub mod error;
pub mod gc;
pub mod models;
pub mod pull;

pub use error::{invalid_request, ollama_http_err, ollama_unreachable_err};
