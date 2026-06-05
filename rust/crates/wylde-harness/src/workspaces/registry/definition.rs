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
/// [`crate::memory::workspaces::store::Workspace`] convention.
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
    #[serde(default)]
    pub rag_enabled: bool,
}

impl WorkspaceDefinition {
    /// Build a fresh definition from a folder path.
    ///
    /// TODO: derive `id` via the shared slug helper, default `name` to
    /// the basename, stamp `created_at` / `updated_at`.
    pub fn new(_folder: &str) -> Self {
        todo!("workspaces redesign: WorkspaceDefinition::new")
    }
}
