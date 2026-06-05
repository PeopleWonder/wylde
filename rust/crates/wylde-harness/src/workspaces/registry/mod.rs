//! `registry/` — the store of workspace configurations.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Registry/`.
//!
//! This is Aaron's "workspaces memory of some sort that stores each
//! workspace configuration" — the *config* tier, distinct from the
//! per-workspace memory-entries layer in [`super::memory`].
//!
//! It owns two on-disk artifacts under `<data_dir>/workspaces/`:
//!
//! * `workspaces.json` — every [`WorkspaceDefinition`] (id, name,
//!   folder, timestamps, feature toggles). The GUI reads this file
//!   directly; the harness is the only writer.
//! * `state.json` — the [`WorkspaceState`]: the active-workspace pointer
//!   plus the MRU list the InferenceBar dropdown renders.
//!
//! ## Split
//!
//! * [`definition`] — the [`WorkspaceDefinition`] type.
//! * [`persistence`] — JSON load/save for `workspaces.json` (atomic
//!   temp-write + rename, matching [`crate::memory::workspaces::store`]).
//! * [`state`] — the [`WorkspaceState`] type + `state.json` IO + MRU
//!   bookkeeping.

pub mod definition;
pub mod persistence;
pub mod state;

pub use definition::WorkspaceDefinition;
pub use state::WorkspaceState;
