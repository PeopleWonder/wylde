//! Gather a workspace's prompt inputs and render them into slot text.
//!
//! A workspace is *config that shapes prompt building*, and [`gather`] is
//! where the active workspace's persona + notes + RAG scope are resolved
//! into a [`WorkspaceContext`], which [`render_slots`] formats into the
//! block the harness turn driver appends to its system prompt.
//!
//! Relocated from the harness's old `workspaces::prompt::inject` in Slice
//! 0d (the in-process gather is now the `workspaces.gather_prompt` verb).
//! The gather logic is the same; only the module paths changed
//! (`crate::notes` / `crate::persona` / `crate::rag` / `crate::registry`)
//! and the notes embed is now bounded for the hot-path verb budget.

use crate::notes::{query, WorkspaceMemoryQuery};
use crate::persona;
use crate::rag::{self, WorkspaceRagScope};
use crate::registry;

/// Everything a workspace contributes to one chat turn's system prompt,
/// resolved from the active workspace's config + stores.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceContext {
    /// Persona override text (empty = none).
    pub persona: String,

    /// Workspace-layer note snippets, highest-scoring first.
    pub memory_snippets: Vec<String>,

    /// RAG snippets scoped to the workspace folder.
    pub rag_snippets: Vec<String>,

    /// Concept-routing candidate set (concept-routing plan R1) — present only
    /// when the caller asked to route (`route == true`) and the workspace has
    /// centroid-bearing concepts. **R1: logged, never injected** — it does NOT
    /// feed any prompt slot, so it leaves [`is_empty`](Self::is_empty) and the
    /// rendered output untouched (injection is R2).
    pub route_candidates: Option<wylde_concept_routing::CandidateSet>,
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
/// Loads the definition from [`registry`]; gathers persona / notes /
/// RAG per the `*_enabled` toggles; returns an empty context for an
/// unknown (or empty) `workspace_id` so a plain chat turn is unaffected.
///
/// `route` is the concept-routing master toggle, forwarded from the harness
/// (concept-routing plan R0/R1). When `false` the function is **byte-identical
/// to before** — it never touches the routing crate. When `true`, on the
/// RAG-enabled path, it embeds the query **once** and shares that vector
/// between RAG and the router, so routing costs **no extra embed and no extra
/// round-trip** (plan §6.1). R1 only *logs* the resulting candidate set; it
/// feeds no slot.
pub async fn gather(workspace_id: &str, user_message: &str, route: bool) -> WorkspaceContext {
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
        // Bound the query embed: this gather is now an IPC hot-path verb
        // (`workspaces.gather_prompt`, Medium 2s) on the chat turn, so a
        // slow/down embedder must degrade notes ranking to recency-only
        // within budget rather than time the whole gather out — same policy
        // the `workspaces.notes.search` verb uses.
        query::top_entries_bounded(&q, query::EMBED_WRITE_BUDGET)
            .await
            .into_iter()
            .map(|e| e.text)
            .collect()
    };

    let (rag_snippets, route_candidates) = match WorkspaceRagScope::from_definition(&def) {
        // Routing ON + RAG enabled: share ONE embed across RAG and the router.
        // This is the only path that reaches the routing crate.
        Some(scope) if route => match rag::indexer::search::embed_query(&def.id, user_message).await
        {
            Some(query_vec) => {
                let rag = rag::scope::retrieve_with_vec(&scope, &query_vec, user_message);
                // R1: compute the candidate set and LOG it (calibration data).
                // R1.5b: when the relation graph reshaped the activation, also
                // log the before→after proof line. Still LOG-ONLY — injects
                // nothing (that is R2).
                let candidates =
                    crate::concepts::routing_bridge::route_with_vec(&def.id, &query_vec, user_message);
                match &candidates {
                    Some(set) => {
                        tracing::info!(target: "concept_routing", "{}", set.log_line());
                        if set.reshaped_by_relations() {
                            tracing::info!(target: "concept_routing", "{}", set.relation_log_line());
                        }
                    }
                    None => tracing::debug!(
                        target: "concept_routing",
                        "concept-routing: skipped for {} — no centroid-bearing concepts yet",
                        def.id
                    ),
                }
                (rag, candidates)
            }
            // Blank query / embed unreachable → same empty RAG as today; no
            // routing (nothing to share an embed with).
            None => (Vec::new(), None),
        },
        // Routing OFF (or RAG disabled): the exact pre-routing path, untouched.
        Some(scope) => (rag::scope::retrieve(&scope, user_message).await, None),
        None => (Vec::new(), None),
    };

    WorkspaceContext {
        persona,
        memory_snippets,
        rag_snippets,
        route_candidates,
    }
}

/// Render-time ceiling on the persona section (~2k estimated tokens at
/// 4 chars/token). The persona rides every turn and the harness treats the
/// workspace block as one eviction unit — a 10-page persona would crowd out
/// notes + RAG wholesale. Truncation is marked in the rendered text; the
/// stored persona.md is never modified. (Prompt-engineering improvement
/// plan B8.)
const PERSONA_MAX_CHARS: usize = 8_000;

/// Render `ctx` into the system-prompt slot text the harness turn driver
/// appends after its base instruction + tool catalog. Returns an empty
/// string when the context is empty (the caller then appends nothing).
pub fn render_slots(ctx: &WorkspaceContext) -> String {
    if ctx.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n# Workspace context\n");

    let persona = ctx.persona.trim();
    if !persona.is_empty() {
        out.push_str("\n## Persona\n");
        if persona.chars().count() > PERSONA_MAX_CHARS {
            out.extend(persona.chars().take(PERSONA_MAX_CHARS));
            out.push_str(&format!(
                "\n[persona truncated at {PERSONA_MAX_CHARS} characters — shorten persona.md]"
            ));
        } else {
            out.push_str(persona);
        }
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
    use crate::test_support::TestEnv;

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
            ..Default::default()
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
    fn render_slots_caps_an_oversized_persona_with_marker() {
        let ctx = WorkspaceContext {
            persona: "p".repeat(PERSONA_MAX_CHARS + 500),
            memory_snippets: Vec::new(),
            rag_snippets: Vec::new(),
            ..Default::default()
        };
        let s = render_slots(&ctx);
        assert!(s.contains("[persona truncated at"), "marker present: {s}");
        // The rendered persona body is capped (allow for headers + marker).
        assert!(
            s.chars().count() < PERSONA_MAX_CHARS + 200,
            "len {}",
            s.len()
        );

        // An at-cap persona is untouched.
        let ctx = WorkspaceContext {
            persona: "p".repeat(100),
            memory_snippets: Vec::new(),
            rag_snippets: Vec::new(),
            ..Default::default()
        };
        assert!(!render_slots(&ctx).contains("truncated"));
    }

    #[test]
    fn render_slots_omits_absent_sections() {
        let ctx = WorkspaceContext {
            persona: String::new(),
            memory_snippets: vec!["only memory".into()],
            rag_snippets: Vec::new(),
            ..Default::default()
        };
        let s = render_slots(&ctx);
        assert!(!s.contains("## Persona"));
        assert!(s.contains("## Workspace memory"));
        assert!(!s.contains("## Workspace files"));
    }

    #[tokio::test]
    async fn gather_is_empty_for_unknown_or_blank_workspace() {
        let _env = TestEnv::new();
        assert!(gather("", "hi", false).await.is_empty());
        assert!(gather("nope-000000", "hi", false).await.is_empty());
    }

    #[tokio::test]
    async fn gather_includes_persona_when_enabled() {
        let _env = TestEnv::new();
        let def = registry::create("/tmp/gather-persona", None);
        registry::update(&def.id, None, Some(true), Some(false)).unwrap();
        persona::save(&def.id, "Answer in haiku.").unwrap();
        let ctx = gather(&def.id, "hello", false).await;
        assert_eq!(ctx.persona, "Answer in haiku.");
        // RAG disabled → no rag snippets; notes empty → none.
        assert!(ctx.rag_snippets.is_empty());
        assert!(ctx.route_candidates.is_none(), "routing off ⇒ no candidates");
    }

    #[tokio::test]
    async fn gather_route_flag_skips_routing_when_rag_disabled() {
        // Routing only engages on the RAG-enabled path (where the embed is
        // shared); RAG-disabled means no shared embed, so routing is skipped
        // even with the flag on — and the rendered output is unchanged.
        let _env = TestEnv::new();
        let def = registry::create("/tmp/gather-route-no-rag", None);
        registry::update(&def.id, None, Some(true), Some(false)).unwrap();
        persona::save(&def.id, "Hi.").unwrap();
        let ctx = gather(&def.id, "hello", true).await;
        assert_eq!(ctx.persona, "Hi.");
        assert!(ctx.route_candidates.is_none(), "no shared embed ⇒ no routing");
    }
}
