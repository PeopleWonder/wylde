//! `memory/` — the per-workspace memory-entries layer.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Memory/`.
//!
//! This is the **middle tier** of the 3-layer memory architecture
//! (long-term / **workspace** / short-term, per memory
//! `wylde_memory_architecture`). Each workspace owns one bucket of
//! curated memory entries; at prompt-build time the highest-scoring
//! entries are injected as a workspace-memory slot.
//!
//! Do NOT confuse this with the relocated `wylde_workspaces::registry`: registry stores
//! *configs*, this stores *memory entries*.
//!
//! Storage: `<data_dir>/workspaces/<workspace_id>/memory.json`.
//!
//! ## Split
//!
//! * [`entry`] — the [`WorkspaceMemoryEntry`] type + per-workspace
//!   `memory.json` IO.
//! * [`query`] — fetch + score entries for prompt injection
//!   ([`WorkspaceMemoryQuery`]).

pub mod entry;
pub mod query;

pub use entry::WorkspaceMemoryEntry;
pub use query::WorkspaceMemoryQuery;
