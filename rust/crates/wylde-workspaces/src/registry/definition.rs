//! [`WorkspaceDefinition`] — one workspace's configuration record.
//!
//! Serialized into the `workspaces` array of
//! `<data_dir>/workspaces/workspaces.json`. The GUI reads this file
//! directly to render the Workspaces panel; the harness is the only
//! writer (through [`super::persistence`]).
//!
//! Scaffold only — fields are the proposed shape, subject to review.

use serde::{Deserialize, Serialize};

/// A single workspace = a folder plus the config that shapes how a chat
/// turn anchored to it is built.
///
/// Timestamps are `f64` Unix epoch seconds to match the existing
/// retired `memory::workspaces::store::Workspace` convention.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceDefinition {
    /// Stable id. Deterministically derived from `folder` (see the
    /// existing `slug_for` helper) so re-adding the same folder is
    /// idempotent.
    pub id: String,

    /// Human-facing display name (defaults to the folder's basename;
    /// user-editable).
    pub name: String,

    /// Absolute path to the workspace folder. The RAG scope and the
    /// file-browser button in the InferenceBar key off this.
    pub folder: String,

    /// Creation time (epoch seconds).
    #[serde(default)]
    pub created_at: f64,

    /// Last-modified time of *this config record* (epoch seconds) — not
    /// the same as last-activated (that lives in [`super::state`]).
    #[serde(default)]
    pub updated_at: f64,

    /// When `true`, [`super::super::persona`] contributes a persona
    /// override to the prompt for this workspace.
    #[serde(default)]
    pub persona_enabled: bool,

    /// When `true`, [`super::super::rag`] scopes retrieval to `folder`
    /// for turns in this workspace.
    #[serde(default = "default_rag_enabled")]
    pub rag_enabled: bool,
}

fn default_rag_enabled() -> bool {
    true
}

impl WorkspaceDefinition {
    /// Build a fresh definition from a folder path.
    ///
    /// `id` is derived deterministically via [`super::slug::slug_for`]
    /// (so re-adding the same folder is idempotent), `name` defaults to
    /// the folder's basename, and both timestamps are stamped now. RAG
    /// is on by default; persona is opt-in.
    pub fn new(folder: &str) -> Self {
        let now = super::epoch_now();
        Self {
            id: super::slug::slug_for(folder),
            name: basename(folder),
            folder: folder.to_owned(),
            created_at: now,
            updated_at: now,
            persona_enabled: false,
            rag_enabled: true,
        }
    }
}

impl WorkspaceDefinition {
    /// Serialize to a `serde_json::Value` for handing straight to the
    /// IPC layer / GUI.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Human-readable display name from a folder path: the last non-empty
/// path component, falling back to the whole string then `"workspace"`.
fn basename(folder: &str) -> String {
    let trimmed = folder.trim_end_matches(['/', '\\']);
    let base = trimmed
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .trim();
    if base.is_empty() {
        "workspace".to_owned()
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_derives_id_name_and_timestamps() {
        let def = WorkspaceDefinition::new("/tmp/My Project");
        assert!(def.id.starts_with("My_Project-"), "id was {}", def.id);
        assert_eq!(def.name, "My Project");
        assert_eq!(def.folder, "/tmp/My Project");
        assert!(def.rag_enabled);
        assert!(!def.persona_enabled);
        assert!(def.created_at > 0.0);
        assert_eq!(def.created_at, def.updated_at);
    }

    #[test]
    fn basename_handles_windows_and_trailing_seps() {
        assert_eq!(basename(r"C:\Users\wylde\proj"), "proj");
        assert_eq!(basename("/home/x/code/"), "code");
        assert_eq!(basename("/"), "workspace");
    }

    #[test]
    fn roundtrips_through_json() {
        let def = WorkspaceDefinition::new("/tmp/proj");
        let s = serde_json::to_string(&def).unwrap();
        let back: WorkspaceDefinition = serde_json::from_str(&s).unwrap();
        assert_eq!(def, back);
    }
}
