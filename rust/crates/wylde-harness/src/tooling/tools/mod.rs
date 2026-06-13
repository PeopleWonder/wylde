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
//! ## Advertised vs registered (the verb cutover)
//!
//! Since the Slice-6 verb cutover the model-facing catalog is just the
//! eight `wylde_*` verbs (group `"verbs"`) plus the four imperative
//! voice device tools (`turn::prompt::SURVIVING_NAMED_TOOLS`). Every
//! other *active* named tool in these modules — `fs.*`, `search.*`,
//! `memory.*`, `rag.*`, `meta.*`, `ollama.*`, `time.*`, `diff.*`, and
//! voice `transcribe`/`synthesize` — is **registered and dispatchable
//! but no longer advertised**. Its functionality is reached through a
//! verb + a resource type (`tooling::resource::resources::*`); the named
//! entry is kept only so an old model-emitted name still resolves
//! through the alias map / salvage path. This is intentional, not an
//! oversight (see `turn/prompt.rs` and the cutover tests in
//! `tooling/tools/verbs.rs`) — auditors should treat these as
//! "keep-unadvertised, backward-compat", not dead code.
//!
//! ## Phase-deferred stubs
//!
//! * [`deferred`] — the handful of tools whose Rust port depends on the
//!   workspace-memory layer (Phase 7) or that are catalog-only by design
//!   (the Phase-11 voice streaming subscriptions). Each is registered
//!   with `phase_<n>_deferred` so the alias map sees them and the model
//!   gets a clean "not yet" error instead of `unknown_tool` confusion.
//!   The dead Phase-6 shell/git/dev and Phase-11 visual "coming soon"
//!   stubs were removed in the 2026-06-05 catalog cleanup.

pub mod deferred;
pub mod diff;
pub mod fs;
pub mod memory;
pub mod meta;
pub mod ollama;
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
    search::register(reg);
    time_tools::register(reg);
    verbs::register(reg);
    voice::register(reg);
    deferred::register(reg);
}
