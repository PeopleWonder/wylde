//! Concept-routing **Augment injection** (concept-routing plan §6.3, R2;
//! relation-model addendum §4.2) — the server-side, impure half of injection,
//! the sibling of [`super::routing_bridge`] and [`super::relations_bridge`].
//!
//! Given the user-curated concept ids and the turn's already-embedded query
//! vector, this builds the two payloads Aaron's "lean C" injects:
//!
//! 1. a **boundary blurb** per concept — `Label — description. (depends on X;
//!    not related to Y, Z.)` drawn from the concept's description plus its
//!    dependency / exclusion edges in the relation graph. This is the model's
//!    novel payload: telling the LLM what *not* to conflate (the exclusion edge
//!    becomes a negative instruction a raw-vector RAG slot can never carry).
//! 2. **member snippets** — for each curated concept,
//!    [`retrieve::select_member_chunks`] (cosine-to-centroid + MMR over the
//!    concept's member files), round-robined across concepts so the injection
//!    isn't all one concept, capped by the token budget.
//!
//! **Augment, never replace:** the result is returned alongside the existing
//! RAG snippets (which still run unchanged) — strictly *more* context than
//! today, never a substitution. An empty curated set ⇒ empty injection ⇒ the
//! turn falls through to today's RAG (Aaron's lock: curated-empty injects
//! nothing).
//!
//! **Removal test:** delete this file + the `concept_context` plumbing and the
//! workspaces service is back to pre-R2 behaviour.

use std::collections::HashSet;

use super::lens;
use super::retrieve::{self, ConceptSnippet};
use super::store;
use crate::rag::indexer::store as rag_store;
use wylde_concept_routing::{NodeRef, RelationKind};

/// The injected concept context: a leading boundary blurb (all curated concepts'
/// boundary lines, joined) and the round-robined member snippets. Returned as a
/// flat `Vec<String>` for the harness's `### Concepts` slot, blurb first so the
/// slot's token-budget eviction sheds snippets before the (cheap, high-signal)
/// boundary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConceptInjection {
    /// `[0]` = the boundary blurb block (one line per concept), then one entry
    /// per member snippet (best-first, round-robined across concepts).
    pub blocks: Vec<String>,
}

impl ConceptInjection {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Snippets pulled per curated concept before round-robin interleaving. Small —
/// the slot is a focused boundary + a few representative chunks, not a dump.
const SNIPPETS_PER_CONCEPT: usize = 3;

/// Hard cap on total member snippets injected across all curated concepts (the
/// token budget on the harness side does the fine-grained eviction; this bounds
/// the work + the slot size up front).
const MAX_TOTAL_SNIPPETS: usize = 8;

/// Build the Augment injection for `curated_ids` in `workspace_id`. Member
/// chunks are ranked by cosine to each concept's **centroid** (the concept-as-
/// RAG-unit, plan §6.3) — the concept was already chosen *for this query* by the
/// curate menu, so within it we surface its most representative members, not the
/// query's nearest chunks (that's the raw RAG slot's job, which still runs).
/// `curated_ids` are already budget-resolved by
/// [`wylde_concept_routing::apply_curation`]; this only assembles the text.
///
/// `scope` is the **R3a scoped lens** (concept-routing plan §6.2): when `Some`
/// (the active file's region, derived by
/// [`wylde_concept_routing::region_for_active_file`]), each concept's member
/// chunks are narrowed to that subtree via the existing [`lens::lens`] —
/// "Authentication ∩ services/vpn" instead of the whole concept. **Behaviour-
/// safe:** `scope == None` (no active file, or `scope_to_active_region` off) ⇒
/// the whole concept, exactly R2; and an **empty intersection** (the concept
/// has no members under the region) falls back to the whole concept rather than
/// silently injecting nothing. The boundary blurb is concept-level (the
/// concept's identity + its dependency/exclusion edges) and is unaffected by the
/// lens — only the *member snippets* narrow.
///
/// Concepts are processed in `curated_ids` order (the apply step ordered them by
/// activation, strongest first). An unknown id is skipped. An empty `curated_ids`
/// (or no resolvable concepts) yields an empty injection.
pub fn inject_curated(
    workspace_id: &str,
    curated_ids: &[String],
    scope: Option<&str>,
) -> ConceptInjection {
    if curated_ids.is_empty() {
        return ConceptInjection::default();
    }

    // Resolve the curated concepts (preserve order, drop unknown ids).
    let all = store::load(workspace_id);
    let concepts: Vec<_> = curated_ids
        .iter()
        .filter_map(|id| all.iter().find(|c| &c.id == id).cloned())
        .collect();
    if concepts.is_empty() {
        return ConceptInjection::default();
    }

    // ── 1. Boundary blurb (one line per concept) ──────────────────────────
    let graph = super::relations_bridge::load(workspace_id);
    // Label resolver: concept id → its label; vocab identifier → the identifier.
    let label_of = |node: &NodeRef| -> String {
        match node {
            NodeRef::Concept { id } => all
                .iter()
                .find(|c| &c.id == id)
                .map(|c| c.label.clone())
                .unwrap_or_else(|| id.clone()),
            NodeRef::Vocab { identifier } => identifier.clone(),
        }
    };
    let mut blurb_lines: Vec<String> = Vec::new();
    for c in &concepts {
        let node = NodeRef::concept(&c.id);
        // Forward dependencies (this concept depends-on X).
        let deps: Vec<String> = graph
            .of_kind(RelationKind::Dependency)
            .filter(|r| r.from == node)
            .map(|r| label_of(&r.to))
            .collect();
        // Exclusions (symmetric — the other endpoint, whichever side `node` is).
        let excludes: Vec<String> = graph
            .of_kind(RelationKind::Negative)
            .filter(|r| r.from == node || r.to == node)
            .map(|r| {
                if r.from == node {
                    label_of(&r.to)
                } else {
                    label_of(&r.from)
                }
            })
            .collect();
        blurb_lines.push(render_blurb(&c.label, &c.description, &deps, &excludes));
    }

    // ── 2. Member snippets (round-robin across concepts, budget-capped) ────
    let chunks = rag_store::load_chunks(workspace_id);
    let per_concept: Vec<Vec<ConceptSnippet>> = concepts
        .iter()
        .map(|c| {
            // R3a scoped lens: narrow the concept's member files to the active
            // region. An empty intersection (nothing under the region) falls
            // back to the whole concept — never silently drop everything.
            let allowed: HashSet<String> = match scope {
                Some(region) => {
                    let scoped: Vec<String> = lens::lens(&c.member_files, Some(region))
                        .into_iter()
                        .cloned()
                        .collect();
                    if scoped.is_empty() {
                        c.member_files.iter().cloned().collect()
                    } else {
                        scoped.into_iter().collect()
                    }
                }
                None => c.member_files.iter().cloned().collect(),
            };
            if allowed.is_empty() {
                return Vec::new();
            }
            retrieve::select_member_chunks(
                c.centroid.as_deref(),
                &chunks,
                &allowed,
                SNIPPETS_PER_CONCEPT,
            )
        })
        .collect();
    let snippets = round_robin(per_concept, MAX_TOTAL_SNIPPETS);

    // Assemble: blurb block first (protected), then the H5 definitional
    // ancestor-chain block (also protected, high-signal), then snippet blocks.
    let mut blocks: Vec<String> = Vec::new();
    if !blurb_lines.is_empty() {
        blocks.push(blurb_lines.join("\n"));
    }
    if let Some(hier) = hierarchy_definition_block(workspace_id, curated_ids) {
        blocks.push(hier);
    }
    for s in snippets {
        blocks.push(render_snippet(&s));
    }
    ConceptInjection { blocks }
}

/// **H5 — the definitional ancestor-chain injection** (definitional-hierarchy
/// plan SS3c): for each curated concept that resolves to a hierarchy node WITH a
/// definition, render `Label — definition — under Parent — under Root` along the
/// node's primary containment path. This is the hierarchy's highest-signal,
/// cheapest slot content — an authored leaf definition + its category chain that
/// raw chunk retrieval structurally cannot produce.
///
/// **Gated + identity-when-off (plan SS3):** returns `None` when the master
/// toggle is OFF (the default), so today's injection is *byte-identical* — the
/// block is never added. `Missing`-definition nodes are skipped (we never inject
/// an empty definition — the invariant surfacing). An empty result ⇒ `None` ⇒ no
/// block, so a curated set with no hierarchy nodes changes nothing.
fn hierarchy_definition_block(workspace_id: &str, curated_ids: &[String]) -> Option<String> {
    use wylde_concept_hierarchy::{HierarchyConfig, NodeId};
    // Identity-when-off: the feature is invisible until explicitly enabled.
    if !HierarchyConfig::current().enabled {
        return None;
    }
    let graph = super::hierarchy_bridge::current_graph(workspace_id);
    let mut lines: Vec<String> = Vec::new();
    for id in curated_ids {
        let nid = NodeId::concept(id);
        let Some(node) = graph.node(&nid) else {
            continue;
        };
        if node.definition.is_missing() {
            continue; // never inject an empty definition (plan SS3 invariant)
        }
        let mut line = format!("{} — {}", node.label, node.definition.text.trim());
        // The primary containment path (ancestor_chain is nearest-first incl.
        // the start node; skip the start, name each ancestor as a category).
        for anc_id in graph.ancestor_chain(&nid).iter().skip(1) {
            if let Some(anc) = graph.node(anc_id) {
                line.push_str(&format!(" — under {}", anc.label));
            }
        }
        lines.push(line);
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Render one concept's boundary line: `Label — description. (depends on X;
/// not related to Y, Z.)`. The parenthetical is omitted when there are no
/// dependency / exclusion edges, so an un-related concept reads plainly.
fn render_blurb(label: &str, description: &str, deps: &[String], excludes: &[String]) -> String {
    let desc = description.trim();
    let mut line = if desc.is_empty() {
        label.to_string()
    } else {
        // Avoid doubling the label when the description already leads with it
        // (concept descriptions are often "Label — summary").
        format!("{label} — {desc}")
    };
    let mut clauses: Vec<String> = Vec::new();
    if !deps.is_empty() {
        clauses.push(format!("depends on {}", deps.join(", ")));
    }
    if !excludes.is_empty() {
        clauses.push(format!("not related to {}", excludes.join(", ")));
    }
    if !clauses.is_empty() {
        line.push_str(&format!(" ({})", clauses.join("; ")));
    }
    line
}

/// Render one member snippet for the slot: `` `path` (lines a-b)\n<body> `` —
/// the same shape the RAG slot uses (so the model reads them uniformly).
fn render_snippet(s: &ConceptSnippet) -> String {
    let loc = if s.start_line == s.end_line {
        format!("line {}", s.start_line)
    } else {
        format!("lines {}-{}", s.start_line, s.end_line)
    };
    format!("`{}` ({loc})\n{}", s.path, s.content.trim())
}

/// Interleave per-concept snippet lists round-robin (one from each concept in
/// turn) up to `cap`, so the injection represents every curated concept rather
/// than front-loading the first. Within a concept, order is preserved
/// (best-first from `select_member_chunks`).
fn round_robin(mut lists: Vec<Vec<ConceptSnippet>>, cap: usize) -> Vec<ConceptSnippet> {
    let mut out: Vec<ConceptSnippet> = Vec::new();
    let mut idx = 0usize;
    loop {
        if out.len() >= cap {
            break;
        }
        let mut drained_any = false;
        for list in lists.iter_mut() {
            if let Some(s) = list.get(idx).cloned() {
                out.push(s);
                drained_any = true;
                if out.len() >= cap {
                    break;
                }
            }
        }
        if !drained_any {
            break;
        }
        idx += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::{Concept, ConceptSource};
    use crate::rag::indexer::store::IndexedChunk;
    use crate::test_support::TestEnv;

    fn concept_with(
        id: &str,
        label: &str,
        desc: &str,
        files: &[&str],
        centroid: Vec<f32>,
    ) -> Concept {
        let mut c = Concept::new(id, label, desc, ConceptSource::Manual);
        c.member_files = files.iter().map(|s| s.to_string()).collect();
        c.centroid = Some(centroid);
        c
    }

    fn chunk(path: &str, idx: u32, v: Vec<f32>) -> IndexedChunk {
        IndexedChunk {
            id: format!("{path}:{idx}"),
            path: path.to_owned(),
            chunk_idx: idx,
            content: format!("body of {path}"),
            mtime: 1.0,
            start_line: idx * 10 + 1,
            end_line: idx * 10 + 9,
            vector: v,
        }
    }

    #[test]
    fn render_blurb_states_dependencies_and_exclusions() {
        let line = render_blurb(
            "Nextcloud",
            "self-hosted file sync",
            &["DDNS".into()],
            &["Wylde".into(), "VPN".into()],
        );
        assert_eq!(
            line,
            "Nextcloud — self-hosted file sync (depends on DDNS; not related to Wylde, VPN)"
        );
    }

    #[test]
    fn render_blurb_omits_empty_parenthetical() {
        let line = render_blurb("Auth", "the auth layer", &[], &[]);
        assert_eq!(line, "Auth — the auth layer");
    }

    #[test]
    fn round_robin_interleaves_and_caps() {
        let a = [chunk("a.rs", 0, vec![1.0]), chunk("a.rs", 1, vec![1.0])];
        let b = [chunk("b.rs", 0, vec![1.0])];
        let to_snip = |c: &IndexedChunk| ConceptSnippet {
            path: c.path.clone(),
            start_line: c.start_line,
            end_line: c.end_line,
            content: c.content.clone(),
            score: 0.0,
        };
        let lists = vec![
            a.iter().map(to_snip).collect::<Vec<_>>(),
            b.iter().map(to_snip).collect::<Vec<_>>(),
        ];
        let out = round_robin(lists, 8);
        // a[0], b[0], a[1] — round-robin, b exhausted after one.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].path, "a.rs");
        assert_eq!(out[1].path, "b.rs");
        assert_eq!(out[2].path, "a.rs");
    }

    #[test]
    fn empty_curation_is_empty_injection() {
        let _env = TestEnv::new();
        assert!(inject_curated("ws-empty-00000", &[], None).is_empty());
    }

    // ── H5: definitional ancestor-chain injection (gated) ─────────────────

    fn hier_enable() {
        wylde_concept_hierarchy::HierarchyConfig::persist(
            wylde_concept_hierarchy::HierarchyConfig { enabled: true },
        )
        .expect("enable hierarchy");
    }
    fn hier_disable() {
        let _ = wylde_concept_hierarchy::HierarchyConfig::persist(
            wylde_concept_hierarchy::HierarchyConfig { enabled: false },
        );
    }

    /// Seed a two-level concept DAG: `token` (child) under `auth` (root).
    fn seed_hier(ws: &str) {
        let mut token = Concept::new(
            "token",
            "Token",
            "a bearer credential",
            ConceptSource::Manual,
        );
        token.parent_concepts = vec!["auth".into()];
        store::save(
            ws,
            &[
                Concept::new(
                    "auth",
                    "Auth",
                    "the authentication layer",
                    ConceptSource::Manual,
                ),
                token,
            ],
        )
        .unwrap();
    }

    #[test]
    fn hierarchy_block_is_none_when_toggle_off() {
        let _env = TestEnv::new();
        hier_disable();
        let ws = "inject-hier-off";
        seed_hier(ws);
        // Identity-when-off: no block, byte-identical to pre-hierarchy injection.
        assert_eq!(hierarchy_definition_block(ws, &["token".into()]), None);
    }

    #[test]
    fn hierarchy_block_renders_definition_and_ancestor_chain_when_on() {
        let _env = TestEnv::new();
        hier_enable();
        let ws = "inject-hier-on0";
        seed_hier(ws);
        let block = hierarchy_definition_block(ws, &["token".into()]).expect("on ⇒ a block");
        assert!(
            block.contains("Token — a bearer credential"),
            "node label + definition"
        );
        assert!(
            block.contains("under Auth"),
            "primary containment chain named"
        );
        hier_disable();
    }

    #[test]
    fn hierarchy_block_skips_missing_definitions() {
        let _env = TestEnv::new();
        hier_enable();
        let ws = "inject-hier-mis";
        // A concept with a blank description ⇒ Missing ⇒ never injected.
        store::save(
            ws,
            &[Concept::new("bare", "Bare", "   ", ConceptSource::Manual)],
        )
        .unwrap();
        assert_eq!(
            hierarchy_definition_block(ws, &["bare".into()]),
            None,
            "missing-definition node is skipped (never inject an empty definition)"
        );
        hier_disable();
    }

    #[tokio::test]
    async fn inject_curated_includes_the_hierarchy_block_when_enabled() {
        let _env = TestEnv::new();
        hier_enable();
        let ws = "inject-hier-int";
        seed_hier(ws);
        let out = inject_curated(ws, &["token".into()], None);
        let joined = out.blocks.join("\n");
        assert!(
            joined.contains("Token — a bearer credential — under Auth"),
            "{joined}"
        );
        hier_disable();
    }

    #[tokio::test]
    async fn injects_blurb_and_member_snippets_for_curated_concepts() {
        let _env = TestEnv::new();
        let ws = "inject-rt-00000";
        store::save(
            ws,
            &[
                concept_with(
                    "nextcloud",
                    "Nextcloud",
                    "self-hosted sync",
                    &["nc.rs"],
                    vec![1.0, 0.0],
                ),
                concept_with("ddns", "DDNS", "dynamic DNS", &["ddns.rs"], vec![1.0, 0.0]),
            ],
        )
        .unwrap();
        // Author Nextcloud depends-on DDNS so the blurb states the boundary.
        super::super::relations_bridge::handle_add(serde_json::json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"nextcloud"},
            "to": {"node":"concept","id":"ddns"},
            "kind": "dependency",
        }))
        .await;
        // Index member chunks for the two files.
        rag_store::save_chunks(
            ws,
            &[
                chunk("nc.rs", 0, vec![1.0, 0.0]),
                chunk("ddns.rs", 0, vec![1.0, 0.0]),
            ],
        )
        .unwrap();

        let out = inject_curated(ws, &["nextcloud".into(), "ddns".into()], None);
        assert!(!out.is_empty());
        // Block 0 is the blurb, and it states the dependency boundary.
        assert!(out.blocks[0].contains("Nextcloud — self-hosted sync (depends on DDNS)"));
        // Member snippets from both files were injected.
        let joined = out.blocks.join("\n");
        assert!(joined.contains("`nc.rs`"));
        assert!(joined.contains("`ddns.rs`"));
    }

    /// R3a: a scoped lens narrows the injected member chunks to the active
    /// file's region — a concept spanning two subsystems injects only the
    /// in-region members.
    #[tokio::test]
    async fn scoped_lens_narrows_member_snippets_to_the_region() {
        let _env = TestEnv::new();
        let ws = "inject-lens-00000";
        // One concept whose members straddle two subsystems.
        store::save(
            ws,
            &[concept_with(
                "auth",
                "Authentication",
                "the auth layer",
                &["services/vpn/auth.rs", "extensions/web/auth.rs"],
                vec![1.0, 0.0],
            )],
        )
        .unwrap();
        rag_store::save_chunks(
            ws,
            &[
                chunk("services/vpn/auth.rs", 0, vec![1.0, 0.0]),
                chunk("extensions/web/auth.rs", 0, vec![1.0, 0.0]),
            ],
        )
        .unwrap();

        // Scoped to the VPN subsystem ⇒ only the VPN member is injected.
        let out = inject_curated(ws, &["auth".into()], Some("services/vpn"));
        let joined = out.blocks.join("\n");
        assert!(joined.contains("`services/vpn/auth.rs`"), "in-region kept");
        assert!(
            !joined.contains("`extensions/web/auth.rs`"),
            "out-of-region member excluded by the lens"
        );
    }

    /// R3a behaviour-safe: an empty intersection (the concept has no members
    /// under the region) falls back to the whole concept — never silently drops
    /// everything.
    #[tokio::test]
    async fn empty_intersection_falls_back_to_whole_concept() {
        let _env = TestEnv::new();
        let ws = "inject-lens-fb-0000";
        store::save(
            ws,
            &[concept_with(
                "auth",
                "Authentication",
                "the auth layer",
                &["services/vpn/auth.rs"],
                vec![1.0, 0.0],
            )],
        )
        .unwrap();
        rag_store::save_chunks(ws, &[chunk("services/vpn/auth.rs", 0, vec![1.0, 0.0])]).unwrap();

        // A region the concept has NO members under ⇒ fall back to the whole
        // concept (the VPN member still injects) rather than nothing.
        let out = inject_curated(ws, &["auth".into()], Some("extensions/web"));
        let joined = out.blocks.join("\n");
        assert!(
            joined.contains("`services/vpn/auth.rs`"),
            "empty intersection ⇒ whole-concept fallback, not silent drop"
        );
    }
}
