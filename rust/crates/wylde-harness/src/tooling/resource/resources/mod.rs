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
//! * **Slice 2 — [`memory`]**: the `memory` resource, delegating to the
//!   existing `memory.*` named-tool handlers.
//! * Slice 3 — rag / graph / tree-sitter.
//! * **Slice 4 — [`fs`]** (this slice): `fs_file` + `fs_dir`, delegating
//!   to the existing `fs.*` + `search.*` named-tool handlers. (The
//!   ollama / time / diff portion of the plan's Slice 4 is a follow-up;
//!   this slice covers the filesystem surface.)
//!
//! Every handler is a thin adapter that reshapes a [`super::ResourceRequest`]
//! into the `args` the existing tool handler expects and calls straight
//! through — no logic is duplicated, and the old named tools stay
//! registered in parallel until the Slice-6 cutover.

use super::ResourceRegistry;

pub mod extensions;
pub mod fs;
pub mod memory;

/// Register every built-in resource cluster. Called by
/// [`super::register_resources`].
///
/// Note: [`extensions`] is **not** registered here — extension resources
/// are sourced from `wylde-extension-bridge` at runtime and inserted into
/// the registry's `RwLock` overlay by the sync task
/// ([`extensions::spawn_sync_task`]), not sealed in at init like the
/// built-in clusters.
pub fn register_all(reg: &mut ResourceRegistry) {
    memory::register_memory_resource(reg);
    fs::register_fs_resources(reg);
}
