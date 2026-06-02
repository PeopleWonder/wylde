//! `tooling/resource/` — the **verb layer**. Tool-registry
//! consolidation Slice 1 (`docs/plans/tool-registry-consolidation.md`).
//!
//! Wylde exposes one internal tool per operation today (~35
//! model-callable `ToolEntry`s, near the 60-tool catalog cap). The verb
//! layer collapses the CRUD/search/execute-shaped tools into eight
//! generic verbs — `describe / list / get / create / update / delete /
//! search / execute` — that take a `resource_type` argument and dispatch
//! through a registry of declarative [`ResourceDefinition`]s. The model
//! learns eight verbs once instead of N tool names, and discovery moves
//! into a `describe` call rather than an always-on catalog.
//!
//! ## What Slice 1 ships (substrate only)
//!
//! * [`ResourceOp`], [`Scope`], [`ResourceRequest`], [`ToolContext`],
//!   [`OpHandler`] + [`op_handler`], [`ResourceDefinition`] — the core
//!   types (`definition`).
//! * [`ResourceRegistry`] (static built-ins + `RwLock` extension
//!   overlay), [`resources`] global, [`ToolsetFilter`] (`registry`).
//! * The fine per-op consent gate ([`op_consent_gate`]) that refines the
//!   coarse verb-level tier/consent flag (`gate`).
//!
//! The eight verb `ToolEntry`s live in
//! [`crate::tooling::tools::verbs`]; they delegate here.
//!
//! **No resources are registered yet** — [`register_resources`] is a
//! no-op. The verbs return empty (`describe`) / `not_found`
//! (`list`/`get`/…) until Slice 2 lights up the memory cluster. Every
//! existing per-tool dispatch, tier gate, and consent path is untouched.

pub mod definition;
pub mod gate;
pub mod registry;

pub use definition::{
    op_handler, OpHandler, OpHandlerFn, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
pub use gate::{op_consent_gate, OpGate};
pub use registry::{resources, ResourceRegistry, ToolsetFilter};

/// Register every built-in resource into the registry. The verb-layer
/// twin of [`crate::tooling::tools::register_all`] — explicit
/// registration, called once by [`ResourceRegistry::default`].
///
/// **Slice 1: no-op.** Resources are registered starting in Slice 2
/// (memory), Slice 3 (rag/graph/tree-sitter), Slice 4 (fs/search/ollama/
/// time/diff). Each adds a `register_<cluster>(reg)` call here, mirroring
/// how `register_all` fans out across the per-tool modules.
pub fn register_resources(_reg: &mut ResourceRegistry) {
    // Intentionally empty in Slice 1. See module docs.
}
