//! Built-in tools — one Rust file per Python group under
//! `Core/harness/tooling/tools/<group>/`.
//!
//! Each module exposes a `register(&mut Registry)` fn that pushes
//! every tool it owns into the catalog. [`register_all`] walks them in
//! the same order Python's `tool_registry` would have surfaced them
//! (alphabetical by group name) so wire-shape parity for `tools.list`
//! falls out naturally.
//!
//! ## Phase 6 active modules
//!
//! * [`fs`] — `read_file`, `list_files`, `write_file`, `edit_file`.
//! * [`diff`] — `show_diff`.
//! * [`search`] — `code_search`, `code_search_files`.
//! * [`meta`] — `tool_search` (catalog discovery against the registry).
//! * [`time_tools`] — `time_now`, `time_format` — self-contained
//!   utilities new in Phase 6.
//!
//! ## Phase-deferred stubs
//!
//! * [`deferred`] — every Python tool whose Rust port depends on the
//!   memory layer (Phase 7), the visual / computer-use layer (Phase 11),
//!   or a shell sandbox decision that hasn't landed yet. Each is
//!   registered with `phase_<n>_deferred` so the alias map sees them
//!   and the model gets a clean "not yet" error instead of
//!   `unknown_tool` confusion.

pub mod deferred;
pub mod diff;
pub mod fs;
pub mod memory;
pub mod meta;
pub mod ollama;
pub mod rag;
pub mod search;
pub mod time_tools;
pub mod verbs;
pub mod voice;

use super::registry::Registry;

/// Register every built-in tool. Called once by [`Registry::default`].
pub fn register_all(reg: &mut Registry) {
    diff::register(reg);
    fs::register(reg);
    memory::register(reg);
    meta::register(reg);
    ollama::register(reg);
    rag::register(reg);
    search::register(reg);
    time_tools::register(reg);
    verbs::register(reg);
    voice::register(reg);
    deferred::register(reg);
}
