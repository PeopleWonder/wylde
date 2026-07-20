//! File IO for per-workspace `definition.json` records.
//!
//! Per the Q5 clean-break layout each workspace owns its own folder
//! `<data_dir>/workspaces/<id>/` holding `definition.json`, `persona.md`,
//! and `memory.jsonl`. This module handles the `definition.json` half
//! plus the shared folder helpers; [`super::state`] owns `index.json`.
//!
//! Atomic-write discipline matches the rest of the harness memory layer:
//! reads tolerate a torn/missing file by returning `None`/empty, writes
//! go to `<path>.tmp` then rename. The harness is the **only writer**.

use std::path::PathBuf;

use super::definition::WorkspaceDefinition;
use crate::common::data_dir;

/// `<data_dir>/workspaces/` — the redesign's storage root. Holds
/// `index.json` plus one `<workspace_id>/` subdir per workspace.
pub fn workspaces_dir() -> PathBuf {
    data_dir().join("workspaces")
}

/// `<data_dir>/workspaces/<id>/` — one workspace's bundle directory.
pub fn workspace_dir(workspace_id: &str) -> PathBuf {
    workspaces_dir().join(workspace_id)
}

/// `<data_dir>/workspaces/<id>/definition.json`.
pub fn definition_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("definition.json")
}

/// Load one workspace's [`WorkspaceDefinition`]. Returns `None` on a
/// missing or torn file (never panics). Decrypts at rest (OI-14).
pub fn load_definition(workspace_id: &str) -> Option<WorkspaceDefinition> {
    let raw =
        wylde_shared::encryption::read_to_string_at_rest(&definition_path(workspace_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Encrypt-at-rest (OI-14) + atomically write a workspace's `definition.json`,
/// creating its bundle directory if needed.
pub fn save_definition(def: &WorkspaceDefinition) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(def).unwrap();
    wylde_shared::encryption::write_at_rest(&definition_path(&def.id), body.as_bytes())
}

/// Remove a workspace's entire bundle directory (`definition.json`,
/// `persona.md`, `memory.jsonl`, …). No-op if the dir is already gone.
pub fn delete_workspace_dir(workspace_id: &str) -> std::io::Result<()> {
    let dir = workspace_dir(workspace_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Load every workspace definition the registry index knows about, in
/// MRU order (most-recent first). Ids with a missing/torn
/// `definition.json` are skipped — the index is the source of truth for
/// *which* workspaces exist, this reconstitutes their configs.
pub fn load_all() -> Vec<WorkspaceDefinition> {
    super::state::load_or_default()
        .mru
        .iter()
        .filter_map(|id| load_definition(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn save_then_load_definition_round_trips() {
        let _env = TestEnv::new();
        let def = WorkspaceDefinition::new("/tmp/persist-me");
        save_definition(&def).unwrap();
        let back = load_definition(&def.id).expect("definition loads");
        assert_eq!(def, back);
    }

    #[test]
    fn load_definition_is_none_when_absent() {
        let _env = TestEnv::new();
        assert!(load_definition("nope-000000").is_none());
    }

    #[test]
    fn delete_workspace_dir_removes_bundle() {
        let _env = TestEnv::new();
        let def = WorkspaceDefinition::new("/tmp/delete-me");
        save_definition(&def).unwrap();
        assert!(workspace_dir(&def.id).exists());
        delete_workspace_dir(&def.id).unwrap();
        assert!(!workspace_dir(&def.id).exists());
        assert!(load_definition(&def.id).is_none());
    }
}
