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
//! * **Slice 3 — [`rag`] + [`treesitter`]**: the RAG / knowledge-graph
//!   surface (`rag_chunk`, `rag`, `rag_feedback`, `rag_miss`,
//!   `rag_chunk_usage`, `rag_graph_stats`, `graph`) plus the tree-sitter
//!   sidecar surface (`code_chunk`, `code_entity`).
//! * **Slice 4 — [`fs`]**: `fs_file` + `fs_dir`, delegating to the
//!   existing `fs.*` + `search.*` named-tool handlers. (The ollama / time
//!   / diff portion of the plan's Slice 4 is a follow-up.)
//!
//! Every handler is a thin adapter that reshapes a [`super::ResourceRequest`]
//! into the `args` the existing tool handler expects and calls straight
//! through — no logic is duplicated, and the old named tools stay
//! registered in parallel until the Slice-6 cutover.
//!
//! ## Flag gate (`WYLDE_HARNESS_VERB_TOOLS`)
//!
//! Built-in resources register **unconditionally** here, matching the
//! Slice-2 precedent: registration is inert because the verb tools that
//! reach them are *dark until the Slice-6 cutover* gated by
//! `WYLDE_HARNESS_VERB_TOOLS`. That flag governs the model-facing
//! advertising (Slice 6) and the runtime extension overlay
//! ([`extensions::spawn_sync_task`]) — not the registration of a
//! definition into the registry. A registration-time gate would diverge
//! from `memory.rs` and break the existing `describe`-lists-resources
//! tests.

use super::ResourceRegistry;

pub mod extensions;
pub mod fs;
pub mod memory;
pub mod rag;
pub mod treesitter;

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
    rag::register_rag_resources(reg);
    treesitter::register_treesitter_resources(reg);
    fs::register_fs_resources(reg);
}
