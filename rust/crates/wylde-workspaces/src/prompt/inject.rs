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
    /// rendered output untouched.
    pub route_candidates: Option<wylde_concept_routing::CandidateSet>,

    /// Concept-routing **R2 Augment injection** (concept-routing plan §6.3) —
    /// the boundary blurb + member snippets for the user-curated concepts. The
    /// harness carries this into a dedicated `### Concepts` system-prompt slot
    /// *alongside* the RAG snippets (Augment, never replace). Empty unless the
    /// caller passed a non-empty curated set; rendered by the harness, NOT by
    /// [`render_slots`] (which owns only the persona/notes/RAG block), so the
    /// `slots` string stays byte-identical to pre-R2 when this is empty.
    pub concept_context: Vec<String>,
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
/// (concept-routing plan R0/R1). When `false` *and* `curated_concepts` is `None`
/// the function is **byte-identical to before** — it never touches the routing
/// crate. When `route` is `true`, on the RAG-enabled path, it embeds the query
/// **once** and shares that vector between RAG and the router, so routing costs
/// **no extra embed and no extra round-trip** (plan §6.1). R1 only *logs* the
/// resulting candidate set.
///
/// `curated_concepts` is the concept-routing **R2** curated set (plan §4): when
/// `Some` (even empty), the user has been through the curate-before-inject menu
/// and these are the concepts to Augment-inject. The shared embed ranks each
/// concept's member chunks; the boundary blurb + member snippets land in
/// [`WorkspaceContext::concept_context`]. `None` ⇒ no injection (R1 behaviour),
/// `Some([])` ⇒ injection explicitly empty (curated to nothing) — both leave
/// `concept_context` empty, so the Augment fallback is today's RAG. Injection,
/// like routing, only engages on the RAG-enabled path (where the embed exists).
pub async fn gather(
    workspace_id: &str,
    user_message: &str,
    route: bool,
    curated_concepts: Option<&[String]>,
) -> WorkspaceContext {
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

    // Routing (R1, log-only) and injection (R2) both need the query embed; we
    // share the ONE embed the RAG path already pays for (plan §6.1). The embed
    // happens only when `route` or a curated set is present, so the pure
    // pre-routing path stays untouched.
    let need_embed = route || curated_concepts.is_some();
    let (rag_snippets, route_candidates, concept_context) = match WorkspaceRagScope::from_definition(
        &def,
    ) {
        // Routing / injection ON + RAG enabled: share ONE embed across RAG,
        // the router, and the curated injection.
        Some(scope) if need_embed => {
            match rag::indexer::search::embed_query(&def.id, user_message).await {
                Some(query_vec) => {
                    let rag = rag::scope::retrieve_with_vec(&scope, &query_vec, user_message);

                    // R1: compute the candidate set and LOG it (calibration
                    // data); R1.5b adds the before→after relation line.
                    // Still LOG-ONLY when `route` — the menu is what injects.
                    let candidates = if route {
                        let c = crate::concepts::routing_bridge::route_with_vec(
                            &def.id,
                            &query_vec,
                            user_message,
                        );
                        match &c {
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
                        c
                    } else {
                        None
                    };

                    // R2 Augment injection: build the boundary blurb +
                    // member snippets for the user-curated concepts, ranking
                    // member chunks against the SAME shared embed. Empty
                    // curated set ⇒ empty injection ⇒ today's RAG fallback.
                    let concept_context = match curated_concepts {
                        Some(ids) if !ids.is_empty() => {
                            // R3a scoped lens: when `scope_to_active_region` is
                            // on and the turn carries an `[active_file: …]`
                            // marker, narrow each curated concept's member
                            // chunks to that file's subsystem. Off / no active
                            // file ⇒ `None` ⇒ whole concept (unchanged from R2).
                            let scope = if wylde_concept_routing::RoutingConfig::current()
                                .scope_to_active_region
                            {
                                active_file_region(user_message)
                            } else {
                                None
                            };
                            let inj = crate::concepts::inject::inject_curated(
                                &def.id,
                                ids,
                                scope.as_deref(),
                            );
                            if !inj.is_empty() {
                                tracing::info!(
                                    target: "concept_routing",
                                    "concept-routing[inject]: ws={} concepts={} blocks={}",
                                    def.id,
                                    ids.len(),
                                    inj.blocks.len()
                                );
                            }
                            inj.blocks
                        }
                        _ => Vec::new(),
                    };

                    (rag, candidates, concept_context)
                }
                // Blank query / embed unreachable → same empty RAG as today;
                // no routing/injection (nothing to share an embed with).
                None => (Vec::new(), None, Vec::new()),
            }
        }
        // Routing OFF + no curation (or RAG disabled): the exact pre-routing
        // path, untouched.
        Some(scope) => (
            rag::scope::retrieve(&scope, user_message).await,
            None,
            Vec::new(),
        ),
        None => (Vec::new(), None, Vec::new()),
    };

    WorkspaceContext {
        persona,
        memory_snippets,
        rag_snippets,
        route_candidates,
        concept_context,
    }
}

/// The concept-routing **R3a** scoped-lens region for this turn: the active
/// file's subsystem, or `None` when the turn carries no `[active_file: …]`
/// marker (or it has no usable directory). The harness folds the active file
/// into the composed query as an `[active_file: PATH]` line (the same marker
/// `routing_bridge::strip_markers` strips); we read it back here and hand the
/// path to the crate's pure [`region_for_active_file`](wylde_concept_routing::region_for_active_file).
fn active_file_region(user_message: &str) -> Option<String> {
    extract_active_file(user_message)
        .as_deref()
        .and_then(wylde_concept_routing::region_for_active_file)
}

/// Pull the `[active_file: PATH]` marker's path out of the composed query.
/// Best-effort line scan (the harness always puts the marker on its own line,
/// behind a `\n\n`); returns the trimmed path, or `None` when absent/blank.
fn extract_active_file(user_message: &str) -> Option<String> {
    for line in user_message.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("[active_file:") {
            let path = rest.trim_end().trim_end_matches(']').trim();
            if !path.is_empty() {
                return Some(path.to_owned());
            }
        }
    }
    None
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
    fn extract_active_file_reads_the_marker() {
        let q = "how does auth work\n\n[active_file: services/vpn/tunnel.rs]\n[anchors: vpn]";
        assert_eq!(
            extract_active_file(q).as_deref(),
            Some("services/vpn/tunnel.rs")
        );
        // No marker ⇒ None (the no-scope, whole-concept path).
        assert_eq!(extract_active_file("plain question"), None);
    }

    #[test]
    fn active_file_region_derives_the_subsystem() {
        let q = "q\n\n[active_file: services/vpn/tunnel.rs]";
        assert_eq!(active_file_region(q).as_deref(), Some("services/vpn"));
        // A bare filename has no subsystem ⇒ no scope.
        assert_eq!(active_file_region("q\n\n[active_file: main.rs]"), None);
        // No marker at all ⇒ no scope.
        assert_eq!(active_file_region("just a question"), None);
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
        assert!(gather("", "hi", false, None).await.is_empty());
        assert!(gather("nope-000000", "hi", false, None).await.is_empty());
    }

    #[tokio::test]
    async fn gather_includes_persona_when_enabled() {
        let _env = TestEnv::new();
        let def = registry::create("/tmp/gather-persona", None).unwrap();
        registry::update(&def.id, None, Some(true), Some(false)).unwrap();
        persona::save(&def.id, "Answer in haiku.").unwrap();
        let ctx = gather(&def.id, "hello", false, None).await;
        assert_eq!(ctx.persona, "Answer in haiku.");
        // RAG disabled → no rag snippets; notes empty → none.
        assert!(ctx.rag_snippets.is_empty());
        assert!(
            ctx.route_candidates.is_none(),
            "routing off ⇒ no candidates"
        );
    }

    #[tokio::test]
    async fn gather_route_flag_skips_routing_when_rag_disabled() {
        // Routing only engages on the RAG-enabled path (where the embed is
        // shared); RAG-disabled means no shared embed, so routing is skipped
        // even with the flag on — and the rendered output is unchanged.
        let _env = TestEnv::new();
        let def = registry::create("/tmp/gather-route-no-rag", None).unwrap();
        registry::update(&def.id, None, Some(true), Some(false)).unwrap();
        persona::save(&def.id, "Hi.").unwrap();
        let ctx = gather(&def.id, "hello", true, None).await;
        assert_eq!(ctx.persona, "Hi.");
        assert!(
            ctx.route_candidates.is_none(),
            "no shared embed ⇒ no routing"
        );
    }

    #[tokio::test]
    async fn gather_curated_concepts_skipped_when_rag_disabled() {
        // R2: injection only engages on the RAG-enabled path (the shared embed).
        // With RAG off, a curated set injects nothing — concept_context stays
        // empty — so the rendered output is byte-identical to base.
        let _env = TestEnv::new();
        let def = registry::create("/tmp/gather-curate-no-rag", None).unwrap();
        registry::update(&def.id, None, Some(true), Some(false)).unwrap();
        persona::save(&def.id, "Hi.").unwrap();
        let ctx = gather(&def.id, "hello", false, Some(&["some-concept".to_owned()])).await;
        assert!(
            ctx.concept_context.is_empty(),
            "no shared embed ⇒ no injection"
        );
    }
}
