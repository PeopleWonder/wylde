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

/// Disk-walk: every workspace id whose bundle directory actually holds a
/// `definition.json` under `<data_dir>/workspaces/`.
///
/// This is the **authoritative existence check** the `index.json` MRU only
/// *caches* (#134). A bundle present here but absent from the index is still
/// a real workspace — before this, nothing ever walked the directory, so a
/// lost or stale index rendered every on-disk bundle permanently invisible.
/// Sorted for deterministic enumeration.
pub fn list_bundle_ids() -> Vec<String> {
    let mut ids = Vec::new();
    let Ok(entries) = std::fs::read_dir(workspaces_dir()) else {
        return ids; // absent root == no workspaces yet
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // A directory is a workspace bundle only if it carries a definition.
        if definition_path(&name).exists() {
            ids.push(name);
        }
    }
    ids.sort();
    ids
}

/// Load every workspace definition that exists **on disk**, discovered by
/// [`list_bundle_ids`] rather than read from the index (#134). This no longer
/// depends on `index.json`, so a lost or damaged index cannot hide a bundle.
/// Order is by id; MRU ordering is applied by [`super::list_all`].
pub fn load_all() -> Vec<WorkspaceDefinition> {
    list_bundle_ids()
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

    /// #134: the disk-walk finds every bundle on disk regardless of any
    /// index, and ignores directories that are not real bundles.
    #[test]
    fn list_bundle_ids_walks_disk_and_requires_a_definition() {
        let _env = TestEnv::new();
        let a = WorkspaceDefinition::new("/tmp/walk-a");
        let b = WorkspaceDefinition::new("/tmp/walk-b");
        save_definition(&a).unwrap();
        save_definition(&b).unwrap();
        // A stray directory with no definition.json must NOT count.
        std::fs::create_dir_all(workspace_dir("not-a-bundle")).unwrap();

        let ids = list_bundle_ids();
        assert!(ids.contains(&a.id), "bundle a must be found");
        assert!(ids.contains(&b.id), "bundle b must be found");
        assert!(
            !ids.contains(&"not-a-bundle".to_owned()),
            "a dir without definition.json is not a bundle"
        );
        // load_all reconstitutes them without consulting the index.
        assert_eq!(load_all().len(), 2);
    }

    #[test]
    fn list_bundle_ids_is_empty_when_root_absent() {
        let _env = TestEnv::new();
        assert!(list_bundle_ids().is_empty());
    }
}
