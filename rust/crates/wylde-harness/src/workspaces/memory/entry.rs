//! [`WorkspaceMemoryEntry`] — one curated workspace-layer memory item.
//!
//! Stored in `<data_dir>/workspaces/<workspace_id>/memory.json`. This is
//! the workspace tier of the 3-layer memory model — narrower than
//! long-term (global) memory, broader than short-term (per-conversation
//! working memory).
//!
//! Scaffold only — no logic yet.

use serde::{Deserialize, Serialize};

/// A single workspace-scoped memory entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceMemoryEntry {
    /// Stable entry id.
    pub id: String,

    /// The remembered fact / insight (the text injected into the
    /// prompt).
    pub text: String,

    /// Creation time (epoch seconds).
    #[serde(default)]
    pub created_at: f64,

    /// Last time this entry was surfaced into a prompt — feeds the
    /// recency component of [`super::query`] scoring.
    #[serde(default)]
    pub last_used_at: f64,
}

/// `<data_dir>/workspaces/<workspace_id>/memory.json`.
pub fn memory_path(_workspace_id: &str) -> std::path::PathBuf {
    todo!("workspaces redesign: workspace memory_path")
}

/// Load every entry for a workspace (fail-soft: empty on missing/torn).
pub fn load(_workspace_id: &str) -> Vec<WorkspaceMemoryEntry> {
    todo!("workspaces redesign: load workspace memory")
}

/// Atomically replace a workspace's memory file.
pub fn save(_workspace_id: &str, _entries: &[WorkspaceMemoryEntry]) -> std::io::Result<()> {
    todo!("workspaces redesign: save workspace memory")
}
