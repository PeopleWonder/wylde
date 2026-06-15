//! Translate a workspace folder into a RAG query scope, and retrieve the
//! folder-scoped snippets the prompt builder injects.
//!
//! The redesign scaffold left [`retrieve`] a pointer-only stub because the
//! file indexer was Python/LanceDB and not yet ported. The Rust indexer
//! now lives in [`super::indexer`], so [`retrieve`] resolves the scope to
//! real snippets: it embeds the user message and k-NN-searches the
//! workspace's index. It stays **fail-soft** — a missing index, an empty
//! workspace, or an unreachable embedder all yield no snippets (never an
//! error), preserving the plain-chat fallback.

use super::indexer::search;

/// The retrieval scope derived from a workspace.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceRagScope {
    /// Stable id of the workspace whose index is searched.
    pub workspace_id: String,

    /// Absolute workspace folder retrieval is bounded to.
    pub folder: String,

    /// Max snippets to inject into the prompt. This is a *budget cap*, not a
    /// fixed count: the search layer's dynamic-k cutoff
    /// ([`super::indexer::search::rank`]) may return fewer (down to zero) when
    /// the score distribution doesn't warrant filling it — a weak/off-topic
    /// query won't pad the slot up to `limit`.
    pub limit: usize,
}

impl WorkspaceRagScope {
    /// Default snippet budget (upper bound) for the workspace-RAG slot. The
    /// dynamic-k cutoff may inject fewer than this on weak/dominated queries.
    pub const DEFAULT_LIMIT: usize = 5;

    /// Build a scope from a workspace definition. Returns `None` when RAG
    /// is disabled for the workspace (`rag_enabled == false`) or the
    /// folder is blank.
    pub fn from_definition(def: &super::super::registry::WorkspaceDefinition) -> Option<Self> {
        if !def.rag_enabled || def.folder.trim().is_empty() {
            return None;
        }
        Some(Self {
            workspace_id: def.id.clone(),
            folder: def.folder.clone(),
            limit: Self::DEFAULT_LIMIT,
        })
    }
}

/// Retrieve scoped snippets for the current turn, formatted for the
/// `## Workspace files` prompt slot.
///
/// Embeds `user_message`, k-NN-searches the workspace index, and renders
/// each hit as a `` `relative/path` (lines a–b) `` header followed by the
/// chunk body. Returns an empty vector when the index is absent/empty or
/// the embedder is unreachable — the slot then contributes nothing.
pub async fn retrieve(scope: &WorkspaceRagScope, user_message: &str) -> Vec<String> {
    let hits = search::query(&scope.workspace_id, user_message, scope.limit).await;
    hits.into_iter().map(|h| render_hit(scope, &h)).collect()
}

/// Format one search hit into a prompt snippet. The path is shown relative
/// to the workspace folder when it sits under it (shorter + less leaky),
/// falling back to the absolute path otherwise.
fn render_hit(scope: &WorkspaceRagScope, hit: &search::SearchHit) -> String {
    let display = relativize(&scope.folder, &hit.file_path);
    let [start, end] = hit.line_range;
    let loc = if start == end {
        format!("line {start}")
    } else {
        format!("lines {start}-{end}")
    };
    format!("`{display}` ({loc})\n{}", hit.content.trim())
}

/// Strip the workspace-folder prefix from `path` for display. Best-effort:
/// returns the original path if it isn't under `folder`.
fn relativize(folder: &str, path: &str) -> String {
    let folder_norm = folder.replace('\\', "/");
    let path_norm = path.replace('\\', "/");
    let prefix = folder_norm.trim_end_matches('/');
    path_norm
        .strip_prefix(prefix)
        .map(|rest| rest.trim_start_matches('/').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or(path_norm)
}

#[cfg(test)]
mod tests {
    use super::super::super::registry::WorkspaceDefinition;
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn from_definition_respects_rag_enabled() {
        let mut def = WorkspaceDefinition::new("/tmp/scoped");
        def.rag_enabled = true;
        let scope = WorkspaceRagScope::from_definition(&def).expect("scope when enabled");
        assert_eq!(scope.folder, "/tmp/scoped");
        assert_eq!(scope.workspace_id, def.id);
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

    #[tokio::test]
    async fn retrieve_is_empty_without_an_index() {
        let _env = TestEnv::new();
        let scope = WorkspaceRagScope {
            workspace_id: "no-index-000000".into(),
            folder: "/tmp/x".into(),
            limit: 5,
        };
        // No index on disk → fail-soft empty, never an error.
        assert!(retrieve(&scope, "anything").await.is_empty());
    }

    #[test]
    fn relativize_strips_folder_prefix_cross_platform() {
        assert_eq!(
            relativize("/home/x/proj", "/home/x/proj/docs/a.md"),
            "docs/a.md"
        );
        assert_eq!(
            relativize(r"C:\proj", r"C:\proj\src\main.rs"),
            "src/main.rs"
        );
        // Not under the folder → unchanged (normalised separators).
        assert_eq!(relativize("/home/x/proj", "/other/a.md"), "/other/a.md");
    }

    #[test]
    fn render_hit_includes_path_and_line_range() {
        let scope = WorkspaceRagScope {
            workspace_id: "w".into(),
            folder: "/proj".into(),
            limit: 5,
        };
        let hit = search::SearchHit {
            file_path: "/proj/notes.md".into(),
            line_range: [3, 9],
            content: "  body text  ".into(),
            score: 0.5,
            chunk_idx: 0,
        };
        let s = render_hit(&scope, &hit);
        assert!(s.contains("`notes.md`"));
        assert!(s.contains("lines 3-9"));
        assert!(s.contains("body text"));
    }
}
