//! [`WorkspaceState`] — active-workspace pointer + MRU list.
//!
//! Persisted to `<data_dir>/workspaces/state.json`, mirroring the
//! `active_conversation.json` pattern in
//! [`crate::memory::conversations::store`]. The MRU list drives the
//! "MRU-5 dropdown" in the InferenceBar.
//!
//! Kept separate from [`super::definition`] so activating a workspace
//! (a hot, frequent write) doesn't rewrite the whole `workspaces.json`
//! registry.
//!
//! Scaffold only — no logic yet.

use serde::{Deserialize, Serialize};

/// Default MRU window the InferenceBar dropdown shows. Per memory
/// `wylde_rag_workspaces`: "MRU-5 dropdown".
pub const MRU_WINDOW_DEFAULT: usize = 5;

/// The mutable selection state, distinct from the immutable-ish
/// per-workspace config records.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceState {
    /// Currently-active workspace id, or `None` for "no workspace"
    /// (a plain chat turn injects no workspace context).
    #[serde(default)]
    pub active_id: Option<String>,

    /// Most-recently-used workspace ids, newest first. The dropdown
    /// renders the first [`MRU_WINDOW_DEFAULT`].
    #[serde(default)]
    pub mru: Vec<String>,
}

/// `<data_dir>/workspaces/state.json`.
pub fn state_path() -> std::path::PathBuf {
    todo!("workspaces redesign: state_path")
}

/// Read the persisted state (active pointer + MRU). Folds any read
/// error to [`WorkspaceState::default`].
pub fn load() -> WorkspaceState {
    todo!("workspaces redesign: load state")
}

/// Persist `state` (best-effort, atomic temp + rename).
pub fn save(_state: &WorkspaceState) -> std::io::Result<()> {
    todo!("workspaces redesign: save state")
}

/// Mark `workspace_id` active: set `active_id` and move it to the head
/// of the MRU list. Returns the updated state.
pub fn set_active(_workspace_id: &str) -> WorkspaceState {
    todo!("workspaces redesign: set_active")
}
