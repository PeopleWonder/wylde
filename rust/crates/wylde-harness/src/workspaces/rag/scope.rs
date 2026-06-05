//! Translate a workspace folder into a RAG query scope.
//!
//! Scaffold only — no logic yet.

/// The retrieval scope derived from a workspace.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceRagScope {
    /// Absolute workspace folder retrieval is bounded to.
    pub folder: String,

    /// Max snippets to inject into the prompt.
    pub limit: usize,
}

impl WorkspaceRagScope {
    /// Default snippet budget for the workspace-RAG slot.
    pub const DEFAULT_LIMIT: usize = 5;

    /// Build a scope from a workspace definition.
    ///
    /// TODO: read `folder` + `rag_enabled` off the definition; return
    /// `None` when RAG is disabled for the workspace.
    pub fn from_definition(
        _def: &super::super::registry::WorkspaceDefinition,
    ) -> Option<Self> {
        todo!("workspaces redesign: WorkspaceRagScope::from_definition")
    }
}

/// Retrieve scoped snippets for the current turn.
///
/// TODO: dispatch the existing RAG search bounded to `scope.folder`
/// (the indexing path is unchanged; this only narrows the query).
/// Returns rendered snippet strings ready for the prompt builder.
pub fn retrieve(_scope: &WorkspaceRagScope, _user_message: &str) -> Vec<String> {
    todo!("workspaces redesign: rag retrieve")
}
