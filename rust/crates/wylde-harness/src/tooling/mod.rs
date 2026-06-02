//! `tooling/` — internal tool registry + runner. Phase 6 of the Wylde
//! Rust migration. Rust port of `Core/harness/tooling/`.
//!
//! ## Scope
//!
//! * [`registry`] — the in-process catalog. Each entry carries a
//!   canonical id, a description (for the model's tool catalog), a
//!   parameter schema, a tier classification (`destructive: bool`), and
//!   either an active handler closure or a "deferred" marker pointing
//!   at the phase that will land the real implementation.
//! * [`runner`] — the dispatcher. Looks up a tool id, applies tier-gate
//!   policy via the registry's `destructive` flag, invokes the handler
//!   (or returns a deferred error), and threads the result back to the
//!   turn loop as a `serde_json::Value`.
//! * [`tools`] — one Rust file per tool group, mirroring Python's
//!   `Core/harness/tooling/tools/<group>/` layout. Self-contained tools
//!   land here in Phase 6; memory/RAG/visual tools register
//!   `phase_*_deferred` stubs so the alias map sees them.
//!
//! ## Alias map
//!
//! [`registry::Registry::alias_map`] returns a `HashMap<String, String>`
//! mapping every snake-case id, dotted name, and inverse-form
//! derivation back to the canonical id. The turn driver feeds this to
//! the salvage parser so model-emitted names like `fs.read_file`,
//! `fs_read_file`, `read_file` all resolve to the same handler.
//!
//! ## Tier gating
//!
//! The runner consults each entry's `destructive` flag against the
//! turn's `device_tier`:
//!
//! * `read_only` — every tool blocked.
//! * `tool_use` (default) — destructive tools blocked.
//! * `destructive_tool_access` — all tools allowed.

pub mod consent;
pub mod registry;
pub mod resource;
pub mod runner;
pub mod tools;

pub use registry::{Registry, ToolEntry};
pub use resource::{resources, ResourceDefinition, ResourceRegistry, ResourceOp};
pub use runner::{dispatch_tool, DispatchOutcome};
