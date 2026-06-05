//! JSON file IO for the workspace registry (`workspaces.json`).
//!
//! Mirrors the atomic-write discipline already used by
//! [`crate::memory::workspaces::store`]: read tolerates a torn/missing
//! file by returning an empty list; write goes to `<path>.tmp` then
//! renames. The harness is the **only writer**; the GUI reads the file
//! directly.
//!
//! Storage root: `<data_dir>/workspaces/` (see
//! [`crate::memory::common::data_dir`]).
//!
//! Scaffold only — no logic yet.

use std::path::PathBuf;

use super::definition::WorkspaceDefinition;

/// `<data_dir>/workspaces/` — the redesign's storage root. Holds
/// `workspaces.json`, `state.json`, and one `<workspace_id>/` subdir per
/// workspace.
///
/// TODO: `crate::memory::common::data_dir().join("workspaces")`.
pub fn workspaces_dir() -> PathBuf {
    todo!("workspaces redesign: workspaces_dir")
}

/// `<data_dir>/workspaces/workspaces.json` — the registry file.
pub fn registry_path() -> PathBuf {
    todo!("workspaces redesign: registry_path")
}

/// Read every [`WorkspaceDefinition`]. Returns an empty list on a
/// missing or torn file (never panics) — matches the existing store's
/// fail-soft semantics.
pub fn load() -> Vec<WorkspaceDefinition> {
    todo!("workspaces redesign: load registry")
}

/// Atomically replace the registry file with `defs`.
pub fn save(_defs: &[WorkspaceDefinition]) -> std::io::Result<()> {
    todo!("workspaces redesign: save registry")
}
