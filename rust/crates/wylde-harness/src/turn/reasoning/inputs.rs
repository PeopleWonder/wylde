//! `PlanInputs` — the grounded-input bundle the PLAN phase's one reasoner
//! call is prompted with (implementation plan §3, slice S3).
//!
//! Every input is assembled **fail-soft**: an unreachable service, a
//! disabled toggle, or an empty store yields an empty block, never an
//! error — grounding is *additive*, the planner runs without it (plan
//! §3.2 "Grounding is additive, never load-bearing").
//!
//! ## Sources (all pre-existing surfaces)
//!
//! | block | source | cost |
//! |---|---|---|
//! | live concepts | the turn's OWN routed [`CandidateSet`] riding out on `GatheredContext.route_candidates` — **no second embed, no second route call** | 0 |
//! | exclusions (IS-NOT) | (a) `Provenance::Inhibited` in the same candidate set; (b) one `workspaces.concepts.relations.graph` read filtered to `kind == negative` | ≤1 IPC read |
//! | concept boundaries | `workspaces.hierarchy.get_node` per activated concept (ancestor ladder + definition); skipped entirely when the hierarchy toggle is off | ≤[`MAX_BOUNDARY_LOOKUPS`] IPC reads |
//! | lessons | long-term records tagged [`REFLECTION_TAG`] — read **directly, without the D2 workspace filter** (see below) | 0 (local store) |
//! | tool catalog | the same live registry catalog `base_system_prompt` uses | 0 |
//! | context digest | the turn's already-rendered `system_slots` | 0 |
//!
//! ## The IS-NOT scalpel (the heart of the grounding)
//!
//! Exclusions are rendered as **explicit "NOT relevant" lines in the
//! prompt**, not silently subtracted from scores. Flat cosine similarity
//! can't tell sibling concepts apart; the user-authored negative edges
//! (`concept_relations.json`) can — and the whole point is that the
//! planner *sees* what was excluded and why, so it doesn't plan into the
//! excluded territory. Both faces are shown: concepts the router actually
//! suppressed this turn (with the before→after activation), and the
//! authored `X IS NOT Y` boundary edges touching the live set.
//!
//! ## D2 relaxation (Aaron, 2026-07-13 — decision 3, do not "fix" back)
//!
//! [`select_lessons`] reads the long-term reflection store directly,
//! INCLUDING on workspace-bound Deep turns, without the D2 workspace
//! confinement the normal gather applies (`context_gather.rs` gates
//! long-term to unbound conversations). Lessons are operational
//! how-to-work knowledge and REFLECT writes them from workspace turns
//! anyway. Authorized + documented in `config.rs`'s module doc and the
//! implementation plan's R7 resolution.

use serde_json::Value;
use wylde_concept_routing::{CandidateSet, NodeRef, Provenance};
use wylde_shared::ipc;

use crate::memory::long_term::reflection::REFLECTION_TAG;
use crate::turn::context_gather::GatheredContext;
use crate::turn::workspace_context::workspaces_service;

/// Cap on lessons injected into the plan prompt (plan §3.1: top-k,
/// default 5, importance × recency — `list_records` already sorts so).
pub const MAX_LESSONS: usize = 5;

/// Cap on per-concept hierarchy ladder lookups — bounds the IPC fan-out on
/// a heavily-routed turn.
pub const MAX_BOUNDARY_LOOKUPS: usize = 6;

/// The grounded-input bundle (scope §2.1's `PlanInputs`, rendered form).
/// Every field is already prompt-ready lines/text so [`render_user_prompt`]
/// is pure string assembly (golden-testable).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanInputs {
    /// The user's ask, verbatim — restated in the prompt as the goal.
    pub goal: String,
    /// "label (score, provenance) — definition" lines, activated
    /// concepts best-first.
    pub live_concepts: Vec<String>,
    /// EXPLICIT "NOT relevant" / "X IS NOT Y" lines — the flat-cosine
    /// scalpel, made visible to the planner.
    pub exclusions: Vec<String>,
    /// "concept → parent → … → root" definition-ladder lines.
    pub boundaries: Vec<String>,
    /// Lessons from past sessions (reflection insights).
    pub lessons: Vec<String>,
    /// "verb — description" lines from the live registry — EXACTLY the set
    /// the executor advertises (see `render_tool_catalog`; issue #25).
    pub tool_catalog: Vec<String>,
    /// "resource_type — display_name (ops: …)" lines: the legal
    /// `resource_type` values for the eight `wylde_*` verbs.
    ///
    /// Empty in legacy (non-verb) mode, where tools are named directly and
    /// there is no `resource_type` to supply.
    ///
    /// The executor learns these at runtime by calling `wylde_describe` with no
    /// argument — the verb guidance says outright that "the legal
    /// `resource_type` values are NOT in this prompt". **The planner cannot do
    /// that**: PLAN is a single call that must emit a complete DAG. Without
    /// this block, grounding the planner in the verb catalog (#25) would only
    /// trade one failure for another — it would name `wylde_get` correctly and
    /// then invent the `resource_type`. Same no-arg `wylde_describe` payload
    /// (`summary_rows`), rendered one line each.
    pub resource_catalog: Vec<String>,
    /// The turn's rendered context slots — the same grounding the
    /// executor sees.
    pub context_digest: String,
    /// S4b auto-escalation only: `tool → error digest` lines for the hard
    /// tool failures that escalated the turn — the planner must route
    /// around these, not re-plan them. Empty on every ordinary PLAN.
    pub failures: Vec<String>,
}

impl PlanInputs {
    /// One-line summary for the grounding `Step` event:
    /// "Grounded plan in 3 concepts, 2 exclusions, 4 lessons".
    pub fn grounding_summary(&self) -> String {
        format!(
            "Grounded plan in {} concept(s), {} exclusion(s), {} lesson(s)",
            self.live_concepts.len(),
            self.exclusions.len(),
            self.lessons.len()
        )
    }
}

/// Assemble the bundle. `gathered` supplies the routed candidate set and
/// the rendered digest; the workspace reads (relations, hierarchy) are
/// fail-soft IPC; lessons and the catalog are local.
pub(crate) async fn gather(
    workspace_id: Option<&str>,
    user_message: &str,
    gathered: &GatheredContext,
) -> PlanInputs {
    let route = gathered.route_candidates.as_ref();

    let live_concepts = route.map(render_live_concepts).unwrap_or_default();

    let mut exclusions = route.map(render_inhibitions).unwrap_or_default();
    let mut boundaries = Vec::new();
    if let (Some(ws), Some(set)) = (workspace_id, route) {
        exclusions.extend(fetch_is_not_edges(ws, set).await);
        boundaries = fetch_boundaries(ws, set).await;
    }

    PlanInputs {
        goal: user_message.to_owned(),
        live_concepts,
        exclusions,
        boundaries,
        lessons: select_lessons(MAX_LESSONS),
        tool_catalog: render_tool_catalog(),
        resource_catalog: render_resource_catalog(),
        context_digest: gathered.system_slots.clone(),
        failures: Vec::new(),
    }
}

// ── live concepts ───────────────────────────────────────────────────────

/// Human word for a provenance value (the activation's "why").
fn provenance_word(p: &Provenance) -> String {
    match p {
        Provenance::Seed => "seed".to_owned(),
        Provenance::SeedLift { from } => format!("lifted by {}", node_name(from)),
        Provenance::Dependency { from, hops } => {
            format!("dependency of {} ({hops} hop(s))", node_name(from))
        }
        Provenance::Positive { from } => format!("co-activated with {}", node_name(from)),
        Provenance::Containment { from, hops } => {
            format!("contained under {} ({hops} hop(s))", node_name(from))
        }
        Provenance::Inhibited { by, .. } => format!("suppressed by {}", node_name(by)),
    }
}

/// A readable name for a relation endpoint: `concept:x` → `x`,
/// `vocab:y` → `{{y}}` (the anchor spelling users author).
fn node_name(n: &NodeRef) -> String {
    match n {
        NodeRef::Concept { id } => id.clone(),
        NodeRef::Vocab { identifier } => format!("{{{{{identifier}}}}}"),
    }
}

/// The activated concepts, best-first, with score + provenance. The
/// definition (when the hierarchy is on) rides the boundaries block —
/// duplicating it here would double-spend prompt tokens.
fn render_live_concepts(set: &CandidateSet) -> Vec<String> {
    set.activated()
        .map(|c| {
            format!(
                "{} ({:.2}, {})",
                c.label,
                c.score,
                provenance_word(&c.provenance)
            )
        })
        .collect()
}

/// Face (a) of the scalpel: concepts the router actually pushed down this
/// turn via a negative edge — shown with the before→after activation so
/// the planner sees the suppression happened, not just its absence.
fn render_inhibitions(set: &CandidateSet) -> Vec<String> {
    set.concepts
        .iter()
        .filter_map(|c| match &c.provenance {
            Provenance::Inhibited { by, raw } => Some(format!(
                "NOT relevant: {} — suppressed by exclusion from {} (activation {:.2} → {:.2})",
                c.label,
                node_name(by),
                raw,
                c.score
            )),
            _ => None,
        })
        .collect()
}

// ── IS-NOT edges (face b: the authored boundaries) ──────────────────────

/// One `workspaces.concepts.relations.graph` read → "X IS NOT Y" lines for
/// every non-dangling negative edge touching a concept in the routed set
/// (activated or suppressed — a boundary on a suppressed sibling is
/// exactly the disambiguation the planner needs). Fail-soft: any error ⇒
/// empty.
async fn fetch_is_not_edges(workspace_id: &str, set: &CandidateSet) -> Vec<String> {
    let reply = ipc::send_action(
        &workspaces_service(),
        "workspaces.concepts.relations.graph",
        serde_json::json!({ "workspace_id": workspace_id }),
    )
    .await;
    if !reply.ok {
        return Vec::new();
    }
    let Some(relations) = reply.data.get("relations").and_then(Value::as_array) else {
        return Vec::new();
    };

    let known: Vec<&str> = set.concepts.iter().map(|c| c.id.as_str()).collect();
    let touches_set =
        |n: &NodeRef| matches!(n, NodeRef::Concept { id } if known.contains(&id.as_str()));
    let label_of = |n: &NodeRef| -> String {
        if let NodeRef::Concept { id } = n {
            if let Some(c) = set.concepts.iter().find(|c| &c.id == id) {
                return c.label.clone();
            }
        }
        node_name(n)
    };

    relations
        .iter()
        .filter_map(|r| {
            if r.get("kind").and_then(Value::as_str) != Some("negative") {
                return None;
            }
            if r.get("dangling").and_then(Value::as_bool).unwrap_or(false) {
                return None;
            }
            let from: NodeRef = serde_json::from_value(r.get("from")?.clone()).ok()?;
            let to: NodeRef = serde_json::from_value(r.get("to")?.clone()).ok()?;
            if !touches_set(&from) && !touches_set(&to) {
                return None;
            }
            let note = r
                .get("note")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            Some(format!(
                "{} IS NOT {}{note}",
                label_of(&from),
                label_of(&to)
            ))
        })
        .collect()
}

// ── containment ladders ─────────────────────────────────────────────────

/// Per activated concept (capped): one `workspaces.hierarchy.get_node`
/// read → "concept → parent → … → root — definition" ladder line. The
/// first `enabled:false` reply short-circuits the rest (toggle off ⇒ the
/// whole block degrades to nothing — OQ-9's concepts-only degrade).
async fn fetch_boundaries(workspace_id: &str, set: &CandidateSet) -> Vec<String> {
    let service = workspaces_service();
    let mut out = Vec::new();
    for c in set.activated().take(MAX_BOUNDARY_LOOKUPS) {
        let reply = ipc::send_action(
            &service,
            "workspaces.hierarchy.get_node",
            serde_json::json!({ "workspace_id": workspace_id, "id": format!("concept:{}", c.id) }),
        )
        .await;
        if !reply.ok {
            continue; // not_found for this node ≠ hierarchy off; keep going
        }
        if reply.data.get("enabled").and_then(Value::as_bool) == Some(false) {
            return Vec::new(); // toggle off — no ladders at all
        }
        let ladder: Vec<String> = std::iter::once(c.label.clone())
            .chain(
                reply
                    .data
                    .get("chain")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|n| n.get("label").and_then(Value::as_str))
                    .map(str::to_owned),
            )
            .collect();
        let definition = reply
            .data
            .get("node")
            .and_then(|n| n.get("definition"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();
        out.push(format!("{}{definition}", ladder.join(" → ")));
    }
    out
}

// ── lessons (D2-relaxed read — see module doc) ──────────────────────────

/// Top-k long-term records tagged `reflection`, importance-then-recency
/// order (exactly `list_records`' sort). Reads the store directly, WITHOUT
/// the D2 workspace filter — the authorized exception (decision 3).
pub fn select_lessons(k: usize) -> Vec<String> {
    crate::memory::long_term::list_records(false)
        .into_iter()
        .filter(|r| r.tags.iter().any(|t| t == REFLECTION_TAG))
        .take(k)
        .map(|r| r.body)
        .collect()
}

// ── tool catalog ────────────────────────────────────────────────────────

/// Compact "verb — description" lines from the live registry (active
/// entries only) — the same catalog the executor's system prompt carries,
/// so the planner only ever names real verbs. The registry lookup + tier
/// gate in the executor remain the dispatch authority.
/// The legal `resource_type` values for the `wylde_*` verbs — one line each,
/// from the same `summary_rows` payload a no-arg `wylde_describe` returns.
///
/// Empty in legacy (non-verb) mode: there are no verbs to feed, so the block is
/// omitted from the prompt entirely rather than rendered empty.
///
/// Deliberately the compact summary (type, name, ops), not full field schemas —
/// R3's "one line each, not full schemas". The planner needs to know a resource
/// EXISTS and which ops it supports to pick a verb; the executor still calls
/// `wylde_describe <type>` for fields at execute time. Full schemas here would
/// blow the PLAN prompt for no planning benefit.
fn render_resource_catalog() -> Vec<String> {
    if !crate::tooling::resource::verb_mode_active() {
        return Vec::new();
    }
    let filter = crate::tooling::resource::ToolsetFilter::all();
    crate::tooling::resource::resources()
        .summary_rows(&filter)
        .into_iter()
        .filter_map(|row| {
            let rt = row.get("resource_type").and_then(Value::as_str)?;
            let name = row
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let ops: Vec<&str> = row
                .get("ops")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut line = rt.to_owned();
            if !name.is_empty() {
                line.push_str(" — ");
                line.push_str(name);
            }
            if !ops.is_empty() {
                line.push_str(&format!(" (ops: {})", ops.join(", ")));
            }
            Some(line)
        })
        .collect()
}

/// The tool names the planner may put in a step's `tool` field.
///
/// **Must be exactly what the executor advertises** — same `status == "active"`
/// filter, same [`crate::turn::prompt::advertise`] predicate against the live
/// `verb_mode`, same [`crate::turn::prompt::MAX_CATALOG_TOOLS`] cap, off the
/// same `catalog_payload`. That is the whole of issue #25:
///
/// This used to filter on `status` alone, so it rendered every active tool's
/// raw `id` (`read_file`, `write_file`, …). But the verb-tool cutover
/// (`WYLDE_HARNESS_VERB_TOOLS`, default **ON** since 2026-06-03) means the
/// executor's turn only advertises the eight `wylde_*` verbs plus a small
/// surviving tail. So the planner proposed `read_file`, the executor dispatched
/// `wylde_get`, and `PlanState::finish_round` — which binds a step's result
/// only to a dispatch of that step's OWN tool, by exact name — never matched.
/// Steps never realised, `expected`/`on_surprise` never evaluated, and the tier
/// was decorative on exactly the multi-step tasks it exists for.
///
/// Note this function's own doc on [`PlanInputs::tool_catalog`] already said
/// "verb — description" lines: the *intent* was always the advertised surface;
/// the code had drifted from it.
fn render_tool_catalog() -> Vec<String> {
    render_tool_catalog_from(
        &crate::tooling::runner::catalog_payload(crate::tooling::registry::global()),
        crate::tooling::resource::verb_mode_active(),
    )
}

/// The pure half of [`render_tool_catalog`], split out so the #25 invariant is
/// testable against a real registry (the global one isn't injectable in a unit
/// test — see `tools::verbs`' catalog tests, which build a local `Registry`).
fn render_tool_catalog_from(catalog: &[Value], verb_mode: bool) -> Vec<String> {
    catalog
        .iter()
        .filter(|t| t.get("status").and_then(Value::as_str) == Some("active"))
        .filter(|t| crate::turn::prompt::advertise(t, verb_mode))
        .take(crate::turn::prompt::MAX_CATALOG_TOOLS)
        .filter_map(|t| {
            let id = t.get("id").and_then(Value::as_str)?.to_owned();
            let desc = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let first_line = desc.lines().next().unwrap_or_default().trim();
            Some(if first_line.is_empty() {
                id
            } else {
                format!("{id} — {first_line}")
            })
        })
        .collect()
}

// ── prompt rendering ────────────────────────────────────────────────────

/// The stable PLAN system prompt. Deliberately free of per-turn content so
/// repeated Deep turns on the reasoner slot share a KV prefix.
pub fn plan_system_prompt() -> String {
    "You are the planning stage of an agentic assistant. Read the goal and \
     grounding, then output ONLY a JSON object — no prose, no code fences — \
     with exactly these fields:\n\
     {\"goal\": string (the restated goal),\n \
      \"steps\": [{\"id\": \"s1\", \"intent\": string, \"tool\": string|null, \
     \"args_template\": object, \"depends_on\": [step ids], \
     \"expected\": {\"predicates\": [], \"assertion\": string, \
     \"on_surprise\": \"replan\"|\"continue\"|\"abort\", \"confidence\": 0.0-1.0}}],\n \
      \"reasoning_trace\": string (one short paragraph),\n \
      \"plan_version\": 1}\n\
     Rules:\n\
     - Use ONLY tool names from the provided catalog; set \"tool\": null for a \
     pure reasoning/synthesis step (no tool call).\n\
     - Keep plans short: at most 8 steps, usually 2-4. If the goal can be \
     answered directly from the context, return \"steps\": [].\n\
     - In args_template, reference an earlier step's result with \
     ${stepid.output} or ${stepid.output.field.path}.\n\
     - expected.assertion states, in one sentence, what a non-surprising \
     result looks like; expected.predicates may declare machine checks: \
     {\"kind\":\"non_empty\"}, {\"kind\":\"json_path_exists\",\"path\":\"/x\"}, \
     {\"kind\":\"json_path_equals\",\"path\":\"/x\",\"value\":...}, \
     {\"kind\":\"contains\",\"needle\":\"...\",\"ci\":true}, \
     {\"kind\":\"count_at_least\",\"path\":\"/x\",\"n\":1}, {\"kind\":\"no_error\"}.\n\
     - Respect the exclusions: anything listed as NOT relevant is out of \
     scope — do not plan steps into it."
        .to_owned()
}

/// Render the volatile user message of the PLAN call from the bundle.
/// Empty blocks are omitted entirely (no broken/empty sections).
pub fn render_user_prompt(inputs: &PlanInputs) -> String {
    let mut s = String::new();
    s.push_str("### Goal\n");
    s.push_str(&inputs.goal);

    let section = |title: &str, lines: &[String], s: &mut String| {
        if lines.is_empty() {
            return;
        }
        s.push_str(&format!("\n\n### {title}\n"));
        for l in lines {
            s.push_str("- ");
            s.push_str(l);
            s.push('\n');
        }
        // Drop the trailing newline so sections join uniformly.
        s.truncate(s.trim_end().len());
    };

    section(
        "Live concepts (this turn's routed activation)",
        &inputs.live_concepts,
        &mut s,
    );
    section(
        "Excluded — NOT relevant (user-authored boundaries)",
        &inputs.exclusions,
        &mut s,
    );
    section(
        "Concept boundaries (containment ladders)",
        &inputs.boundaries,
        &mut s,
    );
    section("Lessons from past sessions", &inputs.lessons, &mut s);
    // S4b auto-escalation: what already failed hard this turn — the
    // reason the plan is being drafted at all. Rendered before the
    // catalog so the planner reads the constraint before picking tools.
    section(
        "Hard tool failures this turn (route around these — do not re-plan them)",
        &inputs.failures,
        &mut s,
    );
    section("Available tools", &inputs.tool_catalog, &mut s);
    // Empty in legacy mode ⇒ `section` omits the block entirely.
    section(
        "Resource types for the wylde_* verbs (use one as `resource_type`)",
        &inputs.resource_catalog,
        &mut s,
    );

    if !inputs.context_digest.trim().is_empty() {
        s.push_str("\n\n### Context digest (what the executor will see)\n");
        s.push_str(inputs.context_digest.trim_end());
    }
    s
}

#[cfg(test)]
mod plan_catalog_matches_executor {
    //! Issue #25 — the planner and the executor must share ONE tool
    //! vocabulary. These run against a real `Registry` (the global one isn't
    //! injectable; same approach as `tools::verbs`' catalog tests).

    use super::render_tool_catalog_from;
    use crate::tooling::registry::Registry;

    fn real_registry() -> Registry {
        let mut reg = Registry::empty();
        crate::tooling::tools::register_all(&mut reg);
        reg
    }

    fn real_catalog() -> Vec<serde_json::Value> {
        crate::tooling::runner::catalog_payload(&real_registry())
    }

    /// The canonical tool ids a dispatched call can actually resolve to, given
    /// what the executor advertises in `verb_mode`.
    ///
    /// This mirrors the executor's real path and is the identifier space the
    /// plan lives in: the executor advertises `name` (often dotted/aliased,
    /// e.g. `ollama.auto_evict_lru`), the model emits that, and `actions.rs`
    /// resolves it through `alias_map` to the CANONICAL id before pushing to
    /// `round_results` — "the plan stores canonical ids; the model emits
    /// dotted/aliased names". So a plan step's `tool` must be a canonical id
    /// that some advertised name resolves to.
    fn advertised_canonical_ids(reg: &Registry, verb_mode: bool) -> Vec<String> {
        let catalog = crate::tooling::runner::catalog_payload(reg);
        let aliases = reg.alias_map();
        crate::turn::prompt::build_tools_field(&catalog, verb_mode)
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
            .map(|name| aliases.get(&name).cloned().unwrap_or(name))
            .collect()
    }

    /// Just the `id` half of each "`id` — description" line.
    fn ids(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.split(" — ").next().unwrap_or(l).to_owned())
            .collect()
    }

    /// **The #25 invariant.** Every tool the planner may name must be one the
    /// executor actually advertises — otherwise `PlanState::finish_round`
    /// (which binds a step's result only to a dispatch of that step's OWN
    /// tool, matched by exact name) can never realise the step, and
    /// `expected`/`on_surprise` never fire.
    ///
    /// Asserted in BOTH modes: the bug was mode-dependent, and the default
    /// flipped to verb mode in the Slice-6 cutover.
    #[test]
    fn planner_never_names_a_tool_the_executor_wont_advertise() {
        let reg = real_registry();
        let catalog = crate::tooling::runner::catalog_payload(&reg);
        for verb_mode in [false, true] {
            let planner = ids(&render_tool_catalog_from(&catalog, verb_mode));
            let reachable = advertised_canonical_ids(&reg, verb_mode);
            assert!(
                !planner.is_empty(),
                "planner catalog empty (verb_mode={verb_mode})"
            );
            for name in &planner {
                assert!(
                    reachable.contains(name),
                    "planner may name `{name}`, but no tool the executor advertises \
                     resolves to it (verb_mode={verb_mode}) — that is issue #25: the \
                     model can never dispatch it, so `finish_round` never binds the \
                     step and `expected`/`on_surprise` never fire. reachable={reachable:?}"
                );
            }
        }
    }

    /// The concrete shape of the regression: in verb mode the planner offers
    /// the `wylde_*` verbs, not the retired named tools it used to emit.
    #[test]
    fn verb_mode_planner_catalog_is_the_verb_surface() {
        let planner = ids(&render_tool_catalog_from(&real_catalog(), true));
        for verb in ["wylde_describe", "wylde_get", "wylde_list", "wylde_search"] {
            assert!(
                planner.iter().any(|p| p == verb),
                "verb-mode planner catalog must offer {verb}; got {planner:?}"
            );
        }
    }

    /// Legacy mode must be untouched — it advertises every active tool, and
    /// the planner should still see all of them there.
    #[test]
    fn legacy_mode_planner_catalog_is_wider_than_verb_mode() {
        let catalog = real_catalog();
        let legacy = ids(&render_tool_catalog_from(&catalog, false));
        let verbs = ids(&render_tool_catalog_from(&catalog, true));
        assert!(
            legacy.len() >= verbs.len(),
            "legacy mode should advertise at least as much as verb mode: \
             legacy={} verb={}",
            legacy.len(),
            verbs.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wylde_concept_routing::RoutedConcept;

    fn set_with(concepts: Vec<RoutedConcept>) -> CandidateSet {
        let activated_count = concepts.iter().filter(|c| c.activated).count();
        CandidateSet {
            query_echo: "q".into(),
            concepts,
            vocabulary: Vec::new(),
            abs_threshold: 0.62,
            chosen_cutoff: 0.62,
            activated_count,
            max_concepts: 5,
        }
    }

    fn concept(id: &str, score: f32, activated: bool, provenance: Provenance) -> RoutedConcept {
        RoutedConcept {
            id: id.into(),
            label: id.into(),
            score,
            seed_score: score,
            provenance,
            activated,
        }
    }

    #[test]
    fn live_concepts_render_score_and_provenance() {
        let set = set_with(vec![
            concept("auth-flow", 0.81, true, Provenance::Seed),
            concept(
                "token-store",
                0.66,
                true,
                Provenance::Dependency {
                    from: NodeRef::concept("auth-flow"),
                    hops: 1,
                },
            ),
            concept("styling", 0.30, false, Provenance::Seed),
        ]);
        let lines = render_live_concepts(&set);
        assert_eq!(lines.len(), 2, "suppressed concepts are not 'live'");
        assert_eq!(lines[0], "auth-flow (0.81, seed)");
        assert_eq!(
            lines[1],
            "token-store (0.66, dependency of auth-flow (1 hop(s)))"
        );
    }

    #[test]
    fn inhibited_concepts_render_as_explicit_not_relevant_lines() {
        // THE scalpel: a suppressed concept is *visible* in the prompt with
        // its before→after activation, never silently subtracted.
        let set = set_with(vec![
            concept("auth-flow", 0.81, true, Provenance::Seed),
            concept(
                "session-cache",
                0.32,
                false,
                Provenance::Inhibited {
                    by: NodeRef::concept("auth-flow"),
                    raw: 0.71,
                },
            ),
        ]);
        let lines = render_inhibitions(&set);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "NOT relevant: session-cache — suppressed by exclusion from auth-flow \
             (activation 0.71 → 0.32)"
        );
    }

    #[test]
    fn vocab_endpoints_render_in_anchor_spelling() {
        assert_eq!(node_name(&NodeRef::vocab("ddns")), "{{ddns}}");
        assert_eq!(node_name(&NodeRef::concept("vpn")), "vpn");
    }

    #[test]
    fn grounding_summary_counts() {
        let inputs = PlanInputs {
            live_concepts: vec!["a".into(), "b".into()],
            exclusions: vec!["x".into()],
            lessons: vec!["l1".into(), "l2".into(), "l3".into()],
            ..PlanInputs::default()
        };
        assert_eq!(
            inputs.grounding_summary(),
            "Grounded plan in 2 concept(s), 1 exclusion(s), 3 lesson(s)"
        );
    }

    #[test]
    fn user_prompt_omits_empty_sections() {
        let inputs = PlanInputs {
            goal: "why is the build red?".into(),
            tool_catalog: vec!["fs.read — read a file".into()],
            resource_catalog: Vec::new(),
            ..PlanInputs::default()
        };
        let p = render_user_prompt(&inputs);
        assert!(p.starts_with("### Goal\nwhy is the build red?"));
        assert!(p.contains("### Available tools\n- fs.read — read a file"));
        assert!(!p.contains("Live concepts"), "empty block omitted");
        assert!(!p.contains("NOT relevant"), "empty block omitted");
        assert!(!p.contains("Context digest"), "empty digest omitted");
        assert!(
            !p.contains("Hard tool failures"),
            "ordinary PLAN carries no failures block (S4b)"
        );
    }

    #[test]
    fn escalation_failures_render_before_the_catalog() {
        // S4b: an auto-escalated PLAN carries the hard failures that
        // triggered it, ahead of the tool catalog.
        let inputs = PlanInputs {
            goal: "list the entries".into(),
            tool_catalog: vec!["fs.read — read a file".into()],
            resource_catalog: Vec::new(),
            failures: vec![
                "voice_mic_chunks {} → [error] phase_11_deferred: not yet ported".into(),
            ],
            ..PlanInputs::default()
        };
        let p = render_user_prompt(&inputs);
        let failures_at = p
            .find("### Hard tool failures this turn (route around these — do not re-plan them)")
            .expect("failures section renders");
        let tools_at = p.find("### Available tools").unwrap();
        assert!(failures_at < tools_at, "failures precede the catalog: {p}");
        assert!(
            p.contains("- voice_mic_chunks {} → [error] phase_11_deferred"),
            "{p}"
        );
    }

    /// GOLDEN: the fully-populated plan prompt rendering. Pins the exact
    /// section order + formatting the reasoner is prompted with (S3
    /// done-when: "golden: plan prompt rendering").
    #[test]
    fn golden_full_plan_prompt() {
        let inputs = PlanInputs {
            goal: "trace the auth token flow".into(),
            live_concepts: vec![
                "auth-flow (0.81, seed)".into(),
                "token-store (0.66, dependency of auth-flow (1 hop(s)))".into(),
            ],
            exclusions: vec![
                "NOT relevant: session-cache — suppressed by exclusion from auth-flow \
                 (activation 0.71 → 0.32)"
                    .into(),
                "auth-flow IS NOT oauth-shim — different subsystem".into(),
            ],
            boundaries: vec!["auth-flow → security → architecture — the login path".into()],
            lessons: vec!["symbols.find on this repo returns dotted ids, not paths".into()],
            tool_catalog: vec![
                "workspaces.symbols.find — find code symbols by token".into(),
                "workspaces.rag_query — semantic search over the workspace".into(),
            ],
            resource_catalog: vec![
                "fs.file — File (ops: get, list, search)".into(),
                "workspace.note — Workspace note (ops: get, list, create)".into(),
            ],
            context_digest: "### Concepts\nauth-flow: the login path".into(),
            failures: Vec::new(),
        };
        let expected = "### Goal\n\
trace the auth token flow\n\
\n\
### Live concepts (this turn's routed activation)\n\
- auth-flow (0.81, seed)\n\
- token-store (0.66, dependency of auth-flow (1 hop(s)))\n\
\n\
### Excluded — NOT relevant (user-authored boundaries)\n\
- NOT relevant: session-cache — suppressed by exclusion from auth-flow (activation 0.71 → 0.32)\n\
- auth-flow IS NOT oauth-shim — different subsystem\n\
\n\
### Concept boundaries (containment ladders)\n\
- auth-flow → security → architecture — the login path\n\
\n\
### Lessons from past sessions\n\
- symbols.find on this repo returns dotted ids, not paths\n\
\n\
### Available tools\n\
- workspaces.symbols.find — find code symbols by token\n\
- workspaces.rag_query — semantic search over the workspace\n\
\n\
### Resource types for the wylde_* verbs (use one as `resource_type`)\n\
- fs.file — File (ops: get, list, search)\n\
- workspace.note — Workspace note (ops: get, list, create)\n\
\n\
### Context digest (what the executor will see)\n\
### Concepts\n\
auth-flow: the login path";
        assert_eq!(render_user_prompt(&inputs), expected);
    }

    #[test]
    fn system_prompt_is_stable_and_names_the_contract() {
        let p = plan_system_prompt();
        assert!(p.contains("\"plan_version\": 1"));
        assert!(p.contains("ONLY a JSON object"));
        assert!(p.contains("NOT relevant"), "exclusion rule is spelled out");
        // Stability: two calls render byte-identically (KV-prefix reuse).
        assert_eq!(p, plan_system_prompt());
    }
}
