//! Translate a workspace folder into a RAG query scope.
//!
//! **Pointer-only (Q6).** The redesign does NOT own RAG index
//! bookkeeping — `rag_state.json` stays with the LanceDB indexer wherever
//! it lives today. This module only derives the *scope* (which folder a
//! turn's retrieval is bounded to) from a [`WorkspaceDefinition`] and
//! exposes the retrieval entrypoint the prompt builder calls.
//!
//! [`retrieve`] is intentionally a stub that returns no snippets: there
//! is no first-class Rust workspace-file search yet (the indexer is
//! Python/LanceDB and the legacy `search_files` path is retired in the
//! clean break). When a Rust folder-scoped search lands it slots in here
//! without changing the prompt-builder contract — `gather` already wires
//! the result into the RAG slot.

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

    /// Build a scope from a workspace definition. Returns `None` when RAG
    /// is disabled for the workspace (`rag_enabled == false`) or the
    /// folder is blank.
    pub fn from_definition(def: &super::super::registry::WorkspaceDefinition) -> Option<Self> {
        if !def.rag_enabled || def.folder.trim().is_empty() {
            return None;
        }
        Some(Self {
            folder: def.folder.clone(),
            limit: Self::DEFAULT_LIMIT,
        })
    }
}

/// Retrieve scoped snippets for the current turn.
///
/// **Pointer-only:** returns an empty vector. The heavy folder-scoped
/// search (LanceDB) is owned by the indexer and not ported to Rust in
/// this redesign — see the module docs and Q6. The signature is the
/// stable seam a future Rust search drops into.
pub fn retrieve(_scope: &WorkspaceRagScope, _user_message: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::registry::WorkspaceDefinition;

    #[test]
    fn from_definition_respects_rag_enabled() {
        let mut def = WorkspaceDefinition::new("/tmp/scoped");
        def.rag_enabled = true;
        let scope = WorkspaceRagScope::from_definition(&def).expect("scope when enabled");
        assert_eq!(scope.folder, "/tmp/scoped");
        assert_eq!(scope.limit, WorkspaceRagScope::DEFAULT_LIMIT);

        def.rag_enabled = false;
        assert!(WorkspaceRagScope::from_definition(&def).is_none());
    }

    #[test]
    fn from_definition_none_for_blank_folder() {
        let mut def = WorkspaceDefinition::new("/tmp/x");
        def.folder = "  ".into();
        assert!(WorkspaceRagScope::from_definition(&def).is_none());
    }

    #[test]
    fn retrieve_is_pointer_only_empty() {
        let scope = WorkspaceRagScope {
            folder: "/tmp/x".into(),
            limit: 5,
        };
        assert!(retrieve(&scope, "anything").is_empty());
    }
}
