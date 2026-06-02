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
//! * The built-in [`ResourceDefinition`]s, one module per cluster, under
//!   [`resources`].
//!
//! The eight verb `ToolEntry`s live in
//! [`crate::tooling::tools::verbs`]; they delegate here.
//!
//! ## Registered resources
//!
//! Slice 2 lights up the **memory** cluster ([`resources::memory`]):
//! `search/get/create/update/delete`, each delegating to the existing
//! `memory.*` named-tool handlers (adapter pattern, no logic duplicated).
//! The old named tools stay registered in parallel until the Slice-6
//! cutover. Every existing per-tool dispatch, tier gate, and consent path
//! is untouched.

pub mod definition;
pub mod gate;
pub mod registry;
pub mod resources;

pub use definition::{
    describe_value, op_handler, DescribeFn, OpHandler, OpHandlerFn, ResourceDefinition, ResourceOp,
    ResourceRequest, Scope, ToolContext,
};
pub use gate::{op_consent_gate, OpGate};
pub use registry::{resources, ResourceRegistry, ToolsetFilter};

/// Register every built-in resource into the registry. The verb-layer
/// twin of [`crate::tooling::tools::register_all`] — explicit
/// registration, called once by [`ResourceRegistry::default`].
///
/// Fans out across the per-cluster modules under [`resources`], mirroring
/// how `register_all` walks the per-tool modules. Slice 2 lights up the
/// `memory` cluster; Slice 3 (rag/graph/tree-sitter) and Slice 4
/// (fs/search/ollama/time/diff) add their `register_<cluster>` calls
/// inside [`resources::register_all`].
pub fn register_resources(reg: &mut ResourceRegistry) {
    resources::register_all(reg);
}
