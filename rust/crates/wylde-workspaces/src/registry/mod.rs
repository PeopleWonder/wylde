//! `registry/` — the store of workspace configurations.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Registry/`.
//!
//! This is Aaron's "workspaces memory of some sort that stores each
//! workspace configuration" — the *config* tier, distinct from the
//! per-workspace memory-entries layer in [`super::memory`].
//!
//! ## On-disk layout (Q5 clean break — see the design doc §4)
//!
//! Per-workspace config is split into its own folder; a single index
//! file carries the active pointer + MRU:
//!
//! ```text
//! <data_dir>/workspaces/
//! ├── index.json              # WorkspaceState: { active_id, mru }
//! └── <workspace_id>/
//!     ├── definition.json     # this workspace's WorkspaceDefinition
//!     ├── persona.md          # persona override (super::super::persona)
//!     └── memory.jsonl        # workspace-memory entries (super::super::memory)
//! ```
//!
//! Because MRU-5 eviction (a hard, static window — Q2) is the only way
//! a workspace leaves the registry, `index.json`'s `mru` list is also
//! the authoritative enumeration of which workspaces exist on disk.
//!
//! ## Split
//!
//! * [`definition`] — the [`WorkspaceDefinition`] type + its
//!   `definition.json` IO.
//! * [`slug`] — `slug_for(folder)` deterministic id derivation (moved
//!   here from the retired `crate::memory::workspaces`).
//! * [`persistence`] — folder helpers + per-workspace `definition.json`
//!   load/save/delete + a registry-wide `load_all`.
//! * [`state`] — the [`WorkspaceState`] (active + MRU-5) + `index.json`
//!   IO + the MRU state machine.

pub mod definition;
pub mod persistence;
pub mod slug;
pub mod state;

pub use definition::WorkspaceDefinition;
pub use slug::slug_for;
pub use state::{WorkspaceState, MRU_WINDOW};

/// Unix epoch seconds, `f64` to match the timestamp convention used by
/// the rest of the harness memory layer.
///
/// Rounded to milliseconds: full `as_secs_f64()` nanosecond precision
/// sits at the f64 significand boundary for epoch-scale values, so it
/// can shift by 1 ULP across a JSON serialize→parse round-trip (which
/// would break exact-equality reload checks). Millisecond precision is
/// far finer than any workspace timestamp needs and serializes to a
/// short, unambiguous decimal that round-trips exactly.
pub fn epoch_now() -> f64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    (secs * 1000.0).round() / 1000.0
}

/// Errors surfaced by the registry facade. The IPC layer maps these to
/// `Reply::err_msg` codes.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// No workspace with that id is registered.
    #[error("workspace not found: {0}")]
    NotFound(String),
}

// ── Registry facade ────────────────────────────────────────────────────
//
// The verb handlers in [`super::api`] drive the registry through these
// functions; they compose `persistence` (per-workspace `definition.json`)
// with `state` (the `index.json` active+MRU machine) and keep the two in
// sync — including deleting the on-disk bundle of any workspace evicted
// past the static MRU-5 window.

/// One workspace's config, or `None` if unregistered.
pub fn get(workspace_id: &str) -> Option<WorkspaceDefinition> {
    persistence::load_definition(workspace_id)
}

/// Up to [`MRU_WINDOW`] workspace definitions in MRU order plus the
/// active id — exactly what the InferenceBar dropdown renders.
pub fn list_mru() -> (Vec<WorkspaceDefinition>, Option<String>) {
    let state = state::load();
    let defs = state
        .mru
        .iter()
        .take(MRU_WINDOW)
        .filter_map(|id| persistence::load_definition(id))
        .collect();
    (defs, state.active_id)
}

/// Register a folder as a workspace (idempotent on the derived id) and
/// promote it to the active/MRU head. Re-registering an existing folder
/// preserves its `created_at` and feature toggles; an explicit `name`
/// overrides the stored display name.
pub fn create(folder: &str, name: Option<&str>) -> WorkspaceDefinition {
    let mut def = WorkspaceDefinition::new(folder);
    if let Some(existing) = persistence::load_definition(&def.id) {
        def.created_at = existing.created_at;
        def.persona_enabled = existing.persona_enabled;
        def.rag_enabled = existing.rag_enabled;
        def.name = existing.name;
    }
    if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
        def.name = n.to_owned();
    }
    def.updated_at = epoch_now();
    let _ = persistence::save_definition(&def);
    promote_and_persist(&def.id);
    def
}

/// Mark an existing workspace active (and bump MRU). `Err(NotFound)`
/// when the id is unknown.
pub fn set_active(workspace_id: &str) -> Result<WorkspaceState, RegistryError> {
    if persistence::load_definition(workspace_id).is_none() {
        return Err(RegistryError::NotFound(workspace_id.to_owned()));
    }
    promote_and_persist(workspace_id);
    Ok(state::load())
}

/// Rename / toggle features. Returns the updated definition, or `None`
/// when the id is unknown.
pub fn update(
    workspace_id: &str,
    name: Option<&str>,
    persona_enabled: Option<bool>,
    rag_enabled: Option<bool>,
) -> Option<WorkspaceDefinition> {
    let mut def = persistence::load_definition(workspace_id)?;
    if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
        def.name = n.to_owned();
    }
    if let Some(p) = persona_enabled {
        def.persona_enabled = p;
    }
    if let Some(r) = rag_enabled {
        def.rag_enabled = r;
    }
    def.updated_at = epoch_now();
    let _ = persistence::save_definition(&def);
    Some(def)
}

/// Remove a workspace from the index and delete its on-disk bundle.
/// Returns `false` if it wasn't registered.
pub fn delete(workspace_id: &str) -> bool {
    let existed = persistence::load_definition(workspace_id).is_some();
    let mut state = state::load();
    state.forget(workspace_id);
    let _ = state::save(&state);
    let _ = persistence::delete_workspace_dir(workspace_id);
    existed
}

/// Promote `id` to the active/MRU head, persist `index.json`, and delete
/// the bundles of any workspaces evicted past the static MRU-5 window.
fn promote_and_persist(workspace_id: &str) -> Vec<String> {
    let mut state = state::load();
    let evicted = state.promote(workspace_id);
    let _ = state::save(&state);
    for victim in &evicted {
        let _ = persistence::delete_workspace_dir(victim);
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn create_registers_activates_and_is_idempotent() {
        let _env = TestEnv::new();
        let folder = _env.ws_path("proj-a");
        let a = create(&folder, None);
        assert_eq!(state::load().active_id.as_deref(), Some(a.id.as_str()));

        // Re-create the same folder: same id, created_at preserved.
        let a2 = create(&folder, Some("Renamed"));
        assert_eq!(a2.id, a.id);
        assert_eq!(a2.created_at, a.created_at);
        assert_eq!(a2.name, "Renamed");
        let (defs, _) = list_mru();
        assert_eq!(defs.len(), 1, "idempotent create must not duplicate");
    }

    #[test]
    fn create_evicts_past_mru_window_and_deletes_bundle() {
        let _env = TestEnv::new();
        let mut first_id = String::new();
        for i in 0..=MRU_WINDOW {
            let def = create(&_env.ws_path(&format!("w{i}")), None);
            if i == 0 {
                first_id = def.id.clone();
            }
        }
        // The first (least-recently-used) workspace is evicted.
        let (defs, _) = list_mru();
        assert_eq!(defs.len(), MRU_WINDOW);
        assert!(get(&first_id).is_none(), "evicted bundle must be gone");
        assert!(!persistence::workspace_dir(&first_id).exists());
    }

    #[test]
    fn set_active_errors_on_unknown_id() {
        let _env = TestEnv::new();
        assert!(matches!(
            set_active("nope-000000"),
            Err(RegistryError::NotFound(_))
        ));
    }

    #[test]
    fn update_toggles_and_renames() {
        let _env = TestEnv::new();
        let def = create(&_env.ws_path("upd"), None);
        let updated = update(&def.id, Some("New Name"), Some(true), Some(false)).unwrap();
        assert_eq!(updated.name, "New Name");
        assert!(updated.persona_enabled);
        assert!(!updated.rag_enabled);
        // Persisted.
        assert_eq!(get(&def.id).unwrap(), updated);
    }

    #[test]
    fn delete_forgets_and_returns_existed() {
        let _env = TestEnv::new();
        let def = create(&_env.ws_path("del-me"), None);
        assert!(delete(&def.id));
        assert!(get(&def.id).is_none());
        assert!(state::load().active_id.is_none());
        assert!(!delete("nope-000000"));
    }
}
