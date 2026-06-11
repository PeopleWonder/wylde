//! `long_term/` — global, cross-workspace, user-visible memory tier.
//!
//! Rust port of `Core/harness/memory/long_term.py` (Phase 7.B slice).
//!
//! ## Design
//!
//! Two parallel stores in lockstep, matching Python:
//!
//! * `long_term.json` — authoritative record list. Identical wire shape
//!   to the Python implementation; the Settings UI reads from here.
//! * `long_term.vec.bin` — vector mirror via
//!   [`crate::memory::vector::VectorStore`]. Pure-Rust, bincode-serialised
//!   single file per data dir. Rebuilt by [`reindex`] from the JSON if
//!   the two ever drift.
//!
//! ## On-disk layout
//!
//! Mirrors Python paths under `<data_dir>/`:
//!
//! ```text
//! <data_dir>/long_term.json     ← authoritative records, JSON
//! <data_dir>/long_term.vec.bin  ← bincode vector mirror (NEW shape; replaces Python's LanceDB folder)
//! ```
//!
//! The vector file replaces Python's `<data_dir>/long_term.lance/`
//! folder. the Wylde user accepted the reindex cost on cutover; see
//! `memory/wylde_phase7b_long_term_shipped.md`.
//!
//! ## Strangler-fig status
//!
//! The Rust implementation does **not** yet drive the Python pipe surface.
//! Slice 7.B ships the Rust functions + tool catalog entries; a future
//! parity-test slice will flip `WYLDE_HARNESS_MEMORY_IMPL=rust` to route
//! `memory.long_term.*` actions through Rust. Until then, the Python
//! `Core/harness/memory/long_term.py` stays canonical at runtime.

mod entries;
mod records;
pub mod reflection;
mod scoring;
mod text_search;

#[cfg(test)]
pub(crate) mod test_support;

pub use entries::{
    core_block, delete, get, history, list_records, save, search, touch, touch_all, update,
    SaveError, SearchHit,
};
pub use records::LongTermMemory;
pub use scoring::{combined_score, heuristic_importance, normalize_importance, DEFAULT_DECAY_DAYS};
pub use text_search::{text_search, TextSearchError};
