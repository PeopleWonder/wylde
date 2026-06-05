//! Gather a workspace's prompt inputs and inject them into the turn.
//!
//! This is the keystone of the redesign: a workspace is *config that
//! shapes prompt building*, and [`gather`] is where the active
//! workspace's persona + memory + RAG scope are resolved into a
//! [`WorkspaceContext`], which [`render_slots`] formats into the block
//! the turn driver appends to the system prompt.

use super::super::memory::{query, WorkspaceMemoryQuery};
use super::super::persona;
use super::super::rag::WorkspaceRagScope;
use super::super::{rag, registry};

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
/// Loads the definition from [`registry`]; gathers persona / memory /
/// RAG per the `*_enabled` toggles; returns an empty context for an
/// unknown (or empty) `workspace_id` so a plain chat turn is unaffected.
pub async fn gather(workspace_id: &str, user_message: &str) -> WorkspaceContext {
    if workspace_id.trim().is_empty() {
        return WorkspaceContext::default();
    }
    let Some(def) = registry::get(workspace_id) else {
        return WorkspaceContext::default();
    };

    let persona = if def.persona_enabled {
        persona::load(&def.id).text
    } else {
        String::new()
    };

    let memory_snippets = {
        let q = WorkspaceMemoryQuery {
            workspace_id: def.id.clone(),
            user_message: user_message.to_owned(),
            limit: WorkspaceMemoryQuery::DEFAULT_LIMIT,
        };
        query::top_entries(&q)
            .await
            .into_iter()
            .map(|e| e.text)
            .collect()
    };

    let rag_snippets = match WorkspaceRagScope::from_definition(&def) {
        Some(scope) => rag::scope::retrieve(&scope, user_message),
        None => Vec::new(),
    };

    WorkspaceContext {
        persona,
        memory_snippets,
        rag_snippets,
    }
}

/// Render `ctx` into the system-prompt slot text appended after the base
/// instruction + tool catalog produced by
/// [`crate::turn::prompt::build_system_prompt`]. Returns an empty string
/// when the context is empty (the caller then appends nothing).
pub fn render_slots(ctx: &WorkspaceContext) -> String {
    if ctx.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n# Workspace context\n");

    if !ctx.persona.trim().is_empty() {
        out.push_str("\n## Persona\n");
        out.push_str(ctx.persona.trim());
        out.push('\n');
    }
    if !ctx.memory_snippets.is_empty() {
        out.push_str("\n## Workspace memory\n");
        for s in &ctx.memory_snippets {
            let s = s.trim();
            if !s.is_empty() {
                out.push_str("- ");
                out.push_str(s);
                out.push('\n');
            }
        }
    }
    if !ctx.rag_snippets.is_empty() {
        out.push_str("\n## Workspace files\n");
        for s in &ctx.rag_snippets {
            let s = s.trim();
            if !s.is_empty() {
                out.push_str(s);
                out.push_str("\n\n");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;

    #[test]
    fn render_slots_empty_for_empty_context() {
        assert_eq!(render_slots(&WorkspaceContext::default()), "");
    }

    #[test]
    fn render_slots_labels_each_present_section() {
        let ctx = WorkspaceContext {
            persona: "Be terse.".into(),
            memory_snippets: vec!["uses pytest".into(), "prefers Rust".into()],
            rag_snippets: vec!["fn main() {}".into()],
        };
        let s = render_slots(&ctx);
        assert!(s.contains("# Workspace context"));
        assert!(s.contains("## Persona"));
        assert!(s.contains("Be terse."));
        assert!(s.contains("## Workspace memory"));
        assert!(s.contains("- uses pytest"));
        assert!(s.contains("## Workspace files"));
        assert!(s.contains("fn main() {}"));
    }

    #[test]
    fn render_slots_omits_absent_sections() {
        let ctx = WorkspaceContext {
            persona: String::new(),
            memory_snippets: vec!["only memory".into()],
            rag_snippets: Vec::new(),
        };
        let s = render_slots(&ctx);
        assert!(!s.contains("## Persona"));
        assert!(s.contains("## Workspace memory"));
        assert!(!s.contains("## Workspace files"));
    }

    #[tokio::test]
    async fn gather_is_empty_for_unknown_or_blank_workspace() {
        let _env = TestEnv::new();
        assert!(gather("", "hi").await.is_empty());
        assert!(gather("nope-000000", "hi").await.is_empty());
    }

    #[tokio::test]
    async fn gather_includes_persona_when_enabled() {
        let _env = TestEnv::new();
        let def = registry::create("/tmp/gather-persona", None);
        registry::update(&def.id, None, Some(true), Some(false)).unwrap();
        persona::save(&def.id, "Answer in haiku.").unwrap();
        let ctx = gather(&def.id, "hello").await;
        assert_eq!(ctx.persona, "Answer in haiku.");
        // RAG disabled → no rag snippets; memory empty → none.
        assert!(ctx.rag_snippets.is_empty());
    }
}
