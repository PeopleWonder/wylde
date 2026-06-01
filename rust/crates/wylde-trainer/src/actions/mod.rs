//! Action handler modules — one per logical group.
//!
//! * [`health`] — `caption.health`, `caption.list_backends`.
//! * [`caption`] — `caption.generate`, `caption.generate_batch`,
//!   `caption.generate_video`.
//!
//! All handlers forward to the Python worker (`Trainer/Caption/rust_worker.py`)
//! and map its `{ok, result, error}` envelope onto the IPC `Reply`.

pub mod caption;
pub mod error;
pub mod health;

pub use error::{invalid_request, require_string, worker_failed, worker_unreachable};

// Re-export under the legacy `actions::*` namespace too — the service
// module's docstrings call out the stable IPC error codes and these
// helpers are the canonical builders.
