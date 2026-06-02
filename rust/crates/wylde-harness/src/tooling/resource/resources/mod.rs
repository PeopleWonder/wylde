//! `tooling/resource/resources/` — the built-in [`ResourceDefinition`]s,
//! one module per resource cluster. The verb-layer twin of
//! [`crate::tooling::tools`] (one file per tool group).
//!
//! Each module exposes a `register_<cluster>_resource(&mut
//! ResourceRegistry)` fn that builds and inserts its definition(s). They
//! are fanned out from [`super::register_resources`], mirroring how
//! [`crate::tooling::tools::register_all`] walks the per-tool modules.
//!
//! ## Migration order (plan §6)
//!
//! * **Slice 2 — [`memory`]** (this slice): the `memory` resource,
//!   delegating to the existing `memory.*` named-tool handlers.
//! * Slice 3 — rag / graph / tree-sitter.
//! * Slice 4 — fs / search / ollama / time / diff.
//!
//! Every handler is a thin adapter that reshapes a [`super::ResourceRequest`]
//! into the `args` the existing tool handler expects and calls straight
//! through — no logic is duplicated, and the old named tools stay
//! registered in parallel until the Slice-6 cutover.

use super::ResourceRegistry;

pub mod memory;

/// Register every built-in resource cluster. Called by
/// [`super::register_resources`].
pub fn register_all(reg: &mut ResourceRegistry) {
    memory::register_memory_resource(reg);
}
