//! Gather a workspace's prompt inputs and inject them into the turn.
//!
//! Scaffold only — no logic yet.

/// Everything a workspace contributes to one chat turn's system prompt,
/// resolved from the active workspace's config + stores.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceContext {
    /// Persona override text (empty = none).
    pub persona: String,

    /// Workspace-layer memory snippets, highest-scoring first.
    pub memory_snippets: Vec<String>,

    /// RAG snippets scoped to the workspace folder.
    pub rag_snippets: Vec<String>,
}

impl WorkspaceContext {
    /// True when the workspace contributes nothing — the caller can skip
    /// the workspace slots entirely.
    pub fn is_empty(&self) -> bool {
        self.persona.trim().is_empty()
            && self.memory_snippets.is_empty()
            && self.rag_snippets.is_empty()
    }
}

/// Resolve the full [`WorkspaceContext`] for `workspace_id` against the
/// current turn's `user_message`.
///
/// TODO: load the definition from [`super::super::registry`]; gather
/// persona / memory / RAG per the `*_enabled` toggles; return an empty
/// context for an unknown or `None` workspace so a plain chat turn is
/// unaffected.
pub fn gather(_workspace_id: &str, _user_message: &str) -> WorkspaceContext {
    todo!("workspaces redesign: gather WorkspaceContext")
}

/// Render `ctx` into the system-prompt slot text appended after the
/// base instruction + tool catalog produced by
/// [`crate::turn::prompt::build_system_prompt`].
///
/// TODO: format persona / workspace-memory / RAG into labelled blocks
/// (matching the deferred `_build_system_prompt_with_slots` shape the
/// Python driver used).
pub fn render_slots(_ctx: &WorkspaceContext) -> String {
    todo!("workspaces redesign: render_slots")
}
