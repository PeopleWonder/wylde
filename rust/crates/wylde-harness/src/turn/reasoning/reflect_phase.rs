//! The REFLECT phase — the in-loop turn critique (implementation plan §5,
//! slice S5). **Reuse the machinery, not the pass**: the background
//! memory-consolidation cycle (`memory/reflection.rs` + the scheduler)
//! stays untouched; this module is a different operation that shares its
//! plumbing — the prompt catalog (`"reasoning.critique"` is a sibling of
//! `"memory.consolidate"`) and the long-term lesson write path
//! (`long_term::save` + [`REFLECTION_TAG`] + the τ=0.92 dedup in
//! [`find_duplicate_insight`]).
//!
//! ## What it does
//!
//! After a plan-guided turn drafts its natural-completion answer (the
//! pre-finalize seam), ONE reasoner call critiques the turn: did the draft
//! satisfy the goal, given the plan, the executed step results and the
//! surprises? The critique is a **typed record** ([`TurnCritique`]),
//! grammar-constrained by [`critique_schema`] on the same
//! `constrained_plan` toggle as PLAN — the S1.5 policy line "REFLECT only
//! if S5 defines a structured lessons record" is now satisfied: the
//! grammar pins the ENVELOPE; the strings inside (gap lines, the lesson
//! sentence) stay free prose, exactly the summary-envelope precedent.
//!
//! ## The lesson loop (why reflection isn't decorative)
//!
//! A surviving [`LessonRecord`] is written to long-term memory through the
//! existing reflection write path — [`REFLECTION_TAG`], importance floor,
//! embed-for-write, and the τ=0.92 near-duplicate check. Because PLAN's
//! lessons selector (`inputs::select_lessons`) reads exactly that tag,
//! **a lesson written on turn N is grounding on turn N+1** — closed loop,
//! pinned by e2e. A lesson learned twice reinforces (touches) the existing
//! record instead of duplicating it.
//!
//! ## Gap round
//!
//! A critique that finds concrete gaps buys **one** extra EXECUTE round
//! (never a replan): the draft answer joins the message tail as an
//! assistant message, the gaps ride a user message, and the loop runs one
//! more natural cycle — bounded by construction (once per turn, and only
//! when `MAX_TOOL_LOOPS` has rounds left).
//!
//! ## Fail-soft (the non-negotiable)
//!
//! A reflection failure must never break a turn: an unreachable reasoner,
//! an unparseable critique, or a failed lesson write each log + emit a
//! visible notice (where user-relevant) and the turn finalizes exactly as
//! it would have without REFLECT.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::events::{TurnEvent, TurnPhase};
use crate::memory::long_term;
use crate::memory::long_term::reflection::{
    find_duplicate_insight, REFLECTION_IMPORTANCE_FLOOR, REFLECTION_TAG,
};
use crate::state::TurnHandle;

use super::config::{ReasoningConfig, ReflectGate};
use super::plan_phase::{self, emit_step, DagCallError};
use super::{surprise, ReasoningState};

/// Output allowance for the critique JSON (rides on top of the tier's
/// think budget, exactly like `PLAN_OUTPUT_BUDGET` — Ollama's
/// `num_predict` caps think + content together). The critique is a small
/// closed object (a bool, ≤3 gap lines, one lesson sentence); half the
/// plan allowance bounds a meltdown without ever cutting a real critique.
pub const CRITIQUE_OUTPUT_BUDGET: u32 = 1_024;

/// Lessons below this model-declared confidence are not written — the
/// schema gives the reasoner an explicit hedge (most turns teach nothing;
/// `lesson: null` or a low confidence are both correct outputs) so
/// half-guesses don't pollute the grounding store.
pub const LESSON_MIN_CONFIDENCE: f64 = 0.6;

/// Cap on the draft-answer excerpt in the critique prompt (chars).
pub const DRAFT_MAX_CHARS: usize = 4_000;

// ── the typed lessons record ─────────────────────────────────────────────

/// What kind of operational knowledge a lesson is. Rides the stored
/// record as a `lesson:<kind>` tag (filterable later) — the record BODY
/// stays the plain lesson sentence so PLAN's existing renderer consumes
/// it unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonKind {
    /// How a tool actually behaves ("symbols.find returns dotted ids").
    ToolBehavior,
    /// A planning tactic that worked or failed.
    Planning,
    /// A fact about this environment / workspace / stack.
    Environment,
    /// How the user wants the assistant to work.
    UserPreference,
}

impl LessonKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LessonKind::ToolBehavior => "tool_behavior",
            LessonKind::Planning => "planning",
            LessonKind::Environment => "environment",
            LessonKind::UserPreference => "user_preference",
        }
    }
}

/// The typed lesson — the thing that makes reflection useful rather than
/// decorative. `text` is the transferable insight as ONE self-contained
/// sentence (free prose inside the constrained envelope); it becomes the
/// long-term record body that [`super::inputs::select_lessons`] resurfaces
/// as PLAN grounding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LessonRecord {
    pub text: String,
    pub kind: LessonKind,
    /// The reasoner's own confidence that this is real, durable knowledge
    /// (0–1). Below [`LESSON_MIN_CONFIDENCE`] the lesson is dropped.
    pub confidence: f64,
}

/// The critique — the REFLECT call's whole typed output (plan §5's
/// `{ok, gaps[], lesson?}` made concrete).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnCritique {
    /// Does the draft answer deliver what the goal asked for, on the
    /// evidence of the executed steps?
    pub goal_satisfied: bool,
    /// Concrete, actionable parts of the goal the draft missed (≤3;
    /// empty when satisfied). Each line is free prose.
    #[serde(default)]
    pub gaps: Vec<String>,
    /// One transferable lesson, or `null` — the correct output for most
    /// turns (mirrors the extractor's empty-lists-are-correct discipline).
    #[serde(default)]
    pub lesson: Option<LessonRecord>,
}

/// The critique's JSON Schema — the `format` value for the REFLECT call.
/// MUST stay field-for-field in lockstep with the serde types above (the
/// tests pin both directions; this Ollama build silently fails OPEN on a
/// bad schema, so the lockstep tests are the only guard).
pub fn critique_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal_satisfied": {"type": "boolean"},
            "gaps": {"type": "array", "items": {"type": "string"}, "maxItems": 3},
            "lesson": {"anyOf": [
                {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "kind": {"enum": [
                            "tool_behavior", "planning",
                            "environment", "user_preference"
                        ]},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                    },
                    "required": ["text", "kind", "confidence"],
                    "additionalProperties": false
                },
                {"type": "null"}
            ]}
        },
        "required": ["goal_satisfied", "gaps", "lesson"],
        "additionalProperties": false
    })
}

/// The `format` for the critique call, or `None` when constrained
/// decoding is toggled off — the same `constrained_plan` gate as PLAN and
/// the L2 verdict (the constrained.rs policy table's REFLECT row, now
/// live: the critique is a structured lessons record, so it qualifies).
fn critique_format() -> Option<Value> {
    ReasoningConfig::current()
        .constrained_plan
        .then(critique_schema)
}

// ── gating ───────────────────────────────────────────────────────────────

/// The REFLECT gate (plan §5, OQ-6 default `MultiToolOnly`): reflection
/// costs a reasoner call, so it runs only where a critique can earn it —
/// plan-guided turns (the caller only reaches here with a
/// [`ReasoningState`]) that actually did multi-step work.
/// `tools_dispatched` is the turn's distinct dispatched-call count
/// (`ToolRoundState.dispatched_hashes`).
pub fn should_reflect(gate: ReflectGate, tools_dispatched: usize) -> bool {
    match gate {
        ReflectGate::Off => false,
        ReflectGate::MultiToolOnly => tools_dispatched >= 2,
        ReflectGate::Always => true,
    }
}

// ── prompt rendering (pure — golden-tested) ─────────────────────────────

/// The critique system prompt — user-tunable through the Settings prompt
/// editor without a rebuild (the `memory.consolidate` pattern).
pub fn critique_system_prompt() -> String {
    crate::prompts::store::effective_prompt("reasoning.critique")
}

/// Render the critique call's user message from the turn's own state:
/// the goal, the (final-version) plan with done/pending marks, every
/// executed step's digest, the surprises, and the draft answer. Pure
/// string assembly over already-digested data — no IPC, no re-gather.
pub(crate) fn render_critique_prompt(state: &ReasoningState, draft_answer: &str) -> String {
    let mut s = String::new();
    s.push_str("### Goal\n");
    s.push_str(&state.plan_inputs.goal);

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
        s.truncate(s.trim_end().len());
    };

    let mut plan_title = format!("Plan (version {}", state.dag.plan_version);
    if state.replans_used > 0 {
        plan_title.push_str(&format!(", {} replan(s) used", state.replans_used));
    }
    if state.abandoned {
        plan_title.push_str(", abandoned — the tail ran as plain ReAct");
    }
    plan_title.push(')');
    let plan_lines: Vec<String> = state
        .dag
        .steps
        .iter()
        .map(|step| {
            let tool = step.tool.as_deref().unwrap_or("synthesis (no tool)");
            let done = if state.completed.contains(&step.id) {
                " · done"
            } else {
                " · pending"
            };
            format!("{} · {} · {tool}{done}", step.id, step.intent)
        })
        .collect();
    section(&plan_title, &plan_lines, &mut s);

    let executed_lines: Vec<String> = state
        .executed_log
        .iter()
        .map(|e| {
            let tool = e.tool.as_deref().unwrap_or("synthesis");
            format!("{} ({tool}) → {}", e.id, e.digest)
        })
        .collect();
    section("Executed step results", &executed_lines, &mut s);
    section("Surprises this turn", &state.surprise_log, &mut s);

    s.push_str("\n\n### Draft answer\n");
    s.push_str(&surprise::digest_value(
        &Value::String(draft_answer.to_owned()),
        DRAFT_MAX_CHARS,
    ));

    s.push_str(
        "\n\n### Instructions\n\
         Critique the turn per your system instructions and output ONLY \
         the critique JSON object.",
    );
    s
}

/// The gap-round tail message: the driver appends the draft answer as an
/// assistant message, then this — the model addresses the gaps in one
/// more ordinary round (tools allowed, the usual gates apply).
pub(crate) fn gap_round_message(gaps: &[String]) -> Value {
    let mut text =
        String::from("[reflection] Your draft answer above leaves the goal unmet in these ways:\n");
    for g in gaps {
        text.push_str("- ");
        text.push_str(g);
        text.push('\n');
    }
    text.push_str(
        "Close these gaps now — use tools if you need more evidence — \
         then give the complete final answer.",
    );
    json!({"role": "user", "content": text})
}

// ── parse ────────────────────────────────────────────────────────────────

/// Parse the (think-stripped) critique body: the same direct → fenced →
/// balanced-brace recovery ladder as the plan parser, into the typed
/// record. Every `Err` is a human-readable reason for the visible notice.
pub fn parse_critique(text: &str) -> Result<TurnCritique, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("reflection returned an empty body".to_owned());
    }
    let mut last_err = "no JSON object found in reflection output".to_owned();
    for c in plan_phase::json_candidates(trimmed) {
        match serde_json::from_str::<TurnCritique>(&c) {
            Ok(mut critique) => {
                critique.gaps.retain(|g| !g.trim().is_empty());
                return Ok(critique);
            }
            Err(e) => last_err = format!("critique JSON did not match the schema: {e}"),
        }
    }
    Err(last_err)
}

// ── the lesson write path (the existing machinery, reused) ──────────────

/// What happened to a lesson at the store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LessonWrite {
    /// A new long-term record was minted.
    Saved(String),
    /// A near-duplicate insight already existed (τ=0.92 cosine) — it was
    /// touched (recency bump), no new record. A lesson learned twice
    /// reinforces, never duplicates.
    Reinforced(String),
    /// Not written (empty text / below the confidence floor).
    Skipped(String),
    /// The store write failed (fail-soft — logged, turn unaffected).
    Failed(String),
}

/// The pure decision half, split from the embed/dedup lookups so it
/// unit-tests with injected values (the `duplicate_for_vector` pattern).
pub(crate) fn persist_lesson_with(
    lesson: &LessonRecord,
    duplicate: Option<String>,
    vector: Option<Vec<f32>>,
) -> LessonWrite {
    let text = lesson.text.trim();
    if text.is_empty() {
        return LessonWrite::Skipped("empty lesson text".to_owned());
    }
    // NaN-safe: only a confidence that PROVABLY clears the floor writes
    // (NaN fails the >= and is skipped).
    let confident = lesson.confidence >= LESSON_MIN_CONFIDENCE;
    if !confident {
        return LessonWrite::Skipped(format!(
            "confidence {:.2} below the {LESSON_MIN_CONFIDENCE} floor",
            lesson.confidence
        ));
    }
    if let Some(existing) = duplicate {
        long_term::touch(&existing);
        return LessonWrite::Reinforced(existing);
    }
    match long_term::save(
        text,
        "reflection:turn_critique",
        Some(REFLECTION_IMPORTANCE_FLOOR as f64),
        vec![
            REFLECTION_TAG.to_owned(),
            format!("lesson:{}", lesson.kind.as_str()),
        ],
        vector,
    ) {
        Ok(r) => LessonWrite::Saved(r.id),
        Err(e) => LessonWrite::Failed(e.to_string()),
    }
}

/// Write one lesson through the existing reflection write path:
/// [`find_duplicate_insight`] (τ=0.92 over [`REFLECTION_TAG`] records,
/// fail-soft on a dead embedder) decides reinforce-vs-save, and a save
/// embeds the text so future dedup and semantic search both see it.
async fn persist_lesson(lesson: &LessonRecord) -> LessonWrite {
    let text = lesson.text.trim();
    let confident = lesson.confidence >= LESSON_MIN_CONFIDENCE; // NaN-safe
    if text.is_empty() || !confident {
        // Cheap pre-check before spending an embed call; the sync half
        // repeats it for its own callers.
        return persist_lesson_with(lesson, None, None);
    }
    let duplicate = find_duplicate_insight(text).await;
    let vector = if duplicate.is_none() {
        crate::memory::embed_write::embed_for_write(text).await
    } else {
        None
    };
    persist_lesson_with(lesson, duplicate, vector)
}

// ── the phase itself ─────────────────────────────────────────────────────

/// What one REFLECT pass hands the driver: the extra reasoner cost for
/// the turn meter, and — when the critique found gaps and the round
/// budget allows — the ready-made gap-round tail message.
#[derive(Debug, Default)]
pub(crate) struct ReflectFlow {
    pub gap_message: Option<Value>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Run the REFLECT critique at the turn's tier. Emits
/// `Phase(Reflecting)`, the verdict step, the lesson step (the user sees
/// the agent learned something), and — gaps + `can_gap_round` permitting —
/// returns the gap-round message. Every failure path returns a flow with
/// no gap message and the turn finalizes verbatim.
pub(crate) async fn run(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    state: &ReasoningState,
    draft_answer: &str,
    can_gap_round: bool,
) -> ReflectFlow {
    let mut flow = ReflectFlow::default();
    handle
        .push_turn_event(TurnEvent::Phase {
            turn_id: turn_id.to_owned(),
            phase: TurnPhase::Reflecting,
        })
        .await;

    let messages = json!([
        {"role": "system", "content": critique_system_prompt()},
        {"role": "user", "content": render_critique_prompt(state, draft_answer)},
    ]);
    let format = critique_format();
    let raw = match plan_phase::tiered_constrained_call(
        cfg,
        handle,
        turn_id,
        state.tier,
        &messages,
        format.as_ref(),
        CRITIQUE_OUTPUT_BUDGET,
    )
    .await
    {
        Ok(raw) => raw,
        Err(DagCallError::Cancelled) => return flow,
        Err(DagCallError::Unavailable(detail)) | Err(DagCallError::Invalid(detail)) => {
            tracing::warn!("reasoning: REFLECT call failed ({detail})");
            emit_step(
                handle,
                turn_id,
                "Reflection unavailable — finalizing",
                Some(detail),
            )
            .await;
            return flow;
        }
    };
    flow.prompt_tokens = raw.prompt_tokens;
    flow.completion_tokens = raw.completion_tokens;

    let critique = match parse_critique(&raw.content) {
        Ok(c) => c,
        Err(reason) => {
            tracing::warn!("reasoning: REFLECT output invalid: {reason}");
            emit_step(
                handle,
                turn_id,
                "Reflection output invalid — finalizing",
                Some(reason),
            )
            .await;
            return flow;
        }
    };

    // The verdict, visibly.
    let elapsed_s = raw.elapsed_ms as f64 / 1000.0;
    if critique.goal_satisfied || critique.gaps.is_empty() {
        emit_step(
            handle,
            turn_id,
            format!("Reflection: the answer covers the goal ({elapsed_s:.1}s)"),
            Some(format!(
                "reasoner {} prompt + {} completion tokens",
                raw.prompt_tokens, raw.completion_tokens
            )),
        )
        .await;
    } else {
        emit_step(
            handle,
            turn_id,
            format!(
                "Reflection: {} gap(s) in the draft answer ({elapsed_s:.1}s)",
                critique.gaps.len()
            ),
            Some(critique.gaps.join("\n")),
        )
        .await;
    }

    // The lesson — write it through the shared path and SHOW it.
    if let Some(lesson) = &critique.lesson {
        match persist_lesson(lesson).await {
            LessonWrite::Saved(_) => {
                emit_step(
                    handle,
                    turn_id,
                    "Lesson learned",
                    Some(format!("[{}] {}", lesson.kind.as_str(), lesson.text.trim())),
                )
                .await;
            }
            LessonWrite::Reinforced(_) => {
                emit_step(
                    handle,
                    turn_id,
                    "Lesson reinforced (already known)",
                    Some(format!("[{}] {}", lesson.kind.as_str(), lesson.text.trim())),
                )
                .await;
            }
            LessonWrite::Skipped(reason) => {
                tracing::debug!("reasoning: lesson skipped ({reason})");
            }
            LessonWrite::Failed(reason) => {
                tracing::warn!("reasoning: lesson write failed ({reason})");
            }
        }
    }

    // The gap round — at most one, and only when the loop has room.
    if !critique.goal_satisfied && !critique.gaps.is_empty() {
        if can_gap_round {
            emit_step(
                handle,
                turn_id,
                "Reflection: one more round to close the gap(s)",
                None,
            )
            .await;
            flow.gap_message = Some(gap_round_message(&critique.gaps));
        } else {
            emit_step(
                handle,
                turn_id,
                "Reflection found gaps but the round budget is spent — finalizing",
                Some(critique.gaps.join("\n")),
            )
            .await;
        }
    }
    flow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;
    use crate::turn::reasoning::config::Depth;
    use crate::turn::reasoning::inputs::PlanInputs;
    use crate::turn::reasoning::PlanOutcome;
    use wylde_reasoning_plan::{ExpectedOutcome, PlanDag, PlanStep};

    fn lesson(text: &str, confidence: f64) -> LessonRecord {
        LessonRecord {
            text: text.to_owned(),
            kind: LessonKind::ToolBehavior,
            confidence,
        }
    }

    // ── schema ↔ serde lockstep (both directions — the only guard on an
    //    Ollama build that silently fails OPEN on bad schemas) ───────────

    #[test]
    fn minimal_schema_conformant_critique_deserializes() {
        // Direction 1 (schema → serde): the minimal admissible objects.
        let with_lesson = json!({
            "goal_satisfied": false,
            "gaps": ["missing the second file"],
            "lesson": {"text": "t", "kind": "planning", "confidence": 0.8}
        });
        let c: TurnCritique = serde_json::from_value(with_lesson).unwrap();
        assert!(!c.goal_satisfied);
        assert_eq!(c.gaps.len(), 1);
        assert_eq!(c.lesson.as_ref().unwrap().kind, LessonKind::Planning);

        let null_lesson = json!({"goal_satisfied": true, "gaps": [], "lesson": null});
        let c: TurnCritique = serde_json::from_value(null_lesson).unwrap();
        assert!(c.goal_satisfied);
        assert!(c.lesson.is_none());

        // Tolerance beyond the grammar: a freehand-degraded backend that
        // drops the defaulted fields still parses.
        let bare = json!({"goal_satisfied": true});
        let c: TurnCritique = serde_json::from_value(bare).unwrap();
        assert!(c.gaps.is_empty() && c.lesson.is_none());
    }

    #[test]
    fn serialized_critique_carries_every_required_key() {
        // Direction 2 (serde → schema): a serialized critique must carry
        // every key the schema requires, in the spelled wire form.
        let c = TurnCritique {
            goal_satisfied: false,
            gaps: vec!["g".into()],
            lesson: Some(lesson("t", 0.9)),
        };
        let v = serde_json::to_value(&c).unwrap();
        let schema = critique_schema();
        for k in schema["required"].as_array().unwrap() {
            assert!(
                v.get(k.as_str().unwrap()).is_some(),
                "critique missing required key {k}"
            );
        }
        let lesson_schema = &schema["properties"]["lesson"]["anyOf"][0];
        for k in lesson_schema["required"].as_array().unwrap() {
            assert!(
                v["lesson"].get(k.as_str().unwrap()).is_some(),
                "lesson missing required key {k}"
            );
        }
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn lesson_kind_enum_mirrors_wire_form() {
        let allowed = critique_schema()["properties"]["lesson"]["anyOf"][0]["properties"]["kind"]
            ["enum"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(allowed.len(), 4, "one enum string per LessonKind variant");
        for kind in [
            LessonKind::ToolBehavior,
            LessonKind::Planning,
            LessonKind::Environment,
            LessonKind::UserPreference,
        ] {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire, json!(kind.as_str()), "as_str matches serde");
            assert!(
                allowed.iter().any(|v| *v == wire),
                "{wire} missing from the schema enum"
            );
        }
    }

    // ── gate matrix (S5 done-when) ──────────────────────────────────────

    #[test]
    fn gate_matrix() {
        for n in [0, 1, 2, 5] {
            assert!(!should_reflect(ReflectGate::Off, n), "Off never reflects");
            assert!(
                should_reflect(ReflectGate::Always, n),
                "Always reflects on every plan-guided turn"
            );
        }
        assert!(!should_reflect(ReflectGate::MultiToolOnly, 0));
        assert!(!should_reflect(ReflectGate::MultiToolOnly, 1));
        assert!(should_reflect(ReflectGate::MultiToolOnly, 2));
        assert!(should_reflect(ReflectGate::MultiToolOnly, 3));
    }

    // ── parse ladder ────────────────────────────────────────────────────

    #[test]
    fn parse_recovers_direct_fenced_and_prose_wrapped() {
        let inner = json!({
            "goal_satisfied": true, "gaps": [], "lesson": null
        })
        .to_string();
        parse_critique(&inner).expect("direct");
        parse_critique(&format!("```json\n{inner}\n```")).expect("fenced");
        parse_critique(&format!("Here you go: {inner} done.")).expect("balanced");
        // Blank gap lines are dropped, not kept as noise.
        let padded = json!({
            "goal_satisfied": false, "gaps": ["real gap", "  "], "lesson": null
        })
        .to_string();
        assert_eq!(parse_critique(&padded).unwrap().gaps, vec!["real gap"]);
    }

    #[test]
    fn parse_garbage_returns_readable_reason() {
        assert!(parse_critique("").is_err());
        assert!(parse_critique("I have no critique.")
            .unwrap_err()
            .contains("no JSON object"));
        assert!(parse_critique("{\"gaps\": []}")
            .unwrap_err()
            .contains("did not match the schema"));
    }

    // ── prompt rendering ────────────────────────────────────────────────

    fn step(id: &str, tool: Option<&str>) -> PlanStep {
        PlanStep {
            id: id.into(),
            intent: format!("do {id}"),
            tool: tool.map(str::to_owned),
            args_template: json!({}),
            depends_on: vec![],
            expected: ExpectedOutcome::trusting(),
        }
    }

    fn state_for_prompt() -> ReasoningState {
        let mut state = ReasoningState::new(
            PlanOutcome {
                dag: PlanDag {
                    goal: "g".into(),
                    steps: vec![step("s1", Some("time_now")), step("s2", None)],
                    reasoning_trace: String::new(),
                    plan_version: 2,
                },
                prompt_tokens: 0,
                completion_tokens: 0,
                elapsed_ms: 0,
                plan_inputs: PlanInputs {
                    goal: "trace the auth token flow".into(),
                    ..PlanInputs::default()
                },
            },
            Depth::Think,
        );
        state.replans_used = 1;
        state.completed.insert("s1".into());
        state
            .executed_log
            .push(crate::turn::reasoning::ExecutedStep {
                id: "s1".into(),
                tool: Some("time_now".into()),
                digest: "\"2026-07-14\"".into(),
            });
        state
            .surprise_log
            .push("s1: 1 expected check(s) failed".into());
        state
    }

    /// GOLDEN: the fully-populated critique prompt (pins section order +
    /// formatting the reasoner is critiqued with).
    #[test]
    fn golden_critique_prompt() {
        let state = state_for_prompt();
        let expected = "### Goal\n\
trace the auth token flow\n\
\n\
### Plan (version 2, 1 replan(s) used)\n\
- s1 · do s1 · time_now · done\n\
- s2 · do s2 · synthesis (no tool) · pending\n\
\n\
### Executed step results\n\
- s1 (time_now) → \"2026-07-14\"\n\
\n\
### Surprises this turn\n\
- s1: 1 expected check(s) failed\n\
\n\
### Draft answer\n\
the token flows via the gateway\n\
\n\
### Instructions\n\
Critique the turn per your system instructions and output ONLY the critique JSON object.";
        assert_eq!(
            render_critique_prompt(&state, "the token flows via the gateway"),
            expected
        );
    }

    #[test]
    fn critique_prompt_omits_empty_sections_and_marks_abandonment() {
        let mut state = ReasoningState::new(
            PlanOutcome {
                dag: PlanDag {
                    goal: "g".into(),
                    steps: vec![],
                    reasoning_trace: String::new(),
                    plan_version: 1,
                },
                prompt_tokens: 0,
                completion_tokens: 0,
                elapsed_ms: 0,
                plan_inputs: PlanInputs {
                    goal: "g".into(),
                    ..PlanInputs::default()
                },
            },
            Depth::Think,
        );
        state.abandoned = true;
        let p = render_critique_prompt(&state, "draft");
        assert!(!p.contains("### Plan ("), "empty plan block omitted: {p}");
        assert!(!p.contains("### Executed step results"), "{p}");
        assert!(!p.contains("### Surprises"), "{p}");
        assert!(p.contains("### Draft answer\ndraft"), "{p}");

        // Abandonment is named when the plan block renders.
        let mut state = state_for_prompt();
        state.abandoned = true;
        assert!(
            render_critique_prompt(&state, "d").contains("abandoned — the tail ran as plain ReAct")
        );
    }

    #[test]
    fn gap_round_message_lists_the_gaps_on_a_user_tail() {
        let m = gap_round_message(&["the second file was never read".into()]);
        assert_eq!(m["role"], "user");
        let text = m["content"].as_str().unwrap();
        assert!(text.starts_with("[reflection] Your draft answer"), "{text}");
        assert!(text.contains("- the second file was never read"), "{text}");
        assert!(text.contains("complete final answer"), "{text}");
    }

    #[test]
    fn system_prompt_resolves_through_the_catalog() {
        let p = critique_system_prompt();
        assert!(
            p.contains("goal_satisfied") && p.contains("lesson"),
            "the catalog default names the schema fields: {p}"
        );
        // Stability for KV-prefix reuse across deep turns.
        assert_eq!(p, critique_system_prompt());
    }

    // ── the lesson write path (dedup / supersession — S5 done-when:
    //    "lesson dedups + supersedes") ───────────────────────────────────

    #[test]
    fn lesson_below_confidence_or_empty_is_skipped() {
        let _env = TestEnv::new();
        assert!(matches!(
            persist_lesson_with(&lesson("  ", 0.9), None, None),
            LessonWrite::Skipped(_)
        ));
        assert!(matches!(
            persist_lesson_with(&lesson("real text", 0.4), None, None),
            LessonWrite::Skipped(_)
        ));
        assert!(matches!(
            persist_lesson_with(&lesson("real text", f64::NAN), None, None),
            LessonWrite::Skipped(_)
        ));
        assert!(
            long_term::list_records(true).is_empty(),
            "skips never touch the store"
        );
    }

    #[test]
    fn lesson_saves_with_reflection_tag_kind_tag_and_floor_importance() {
        let _env = TestEnv::new();
        let w = persist_lesson_with(&lesson("symbols.find returns dotted ids", 0.9), None, None);
        let LessonWrite::Saved(id) = w else {
            panic!("expected Saved, got {w:?}");
        };
        let rec = long_term::get(&id).expect("record present");
        assert_eq!(rec.body, "symbols.find returns dotted ids");
        assert_eq!(rec.source, "reflection:turn_critique");
        assert_eq!(rec.importance, REFLECTION_IMPORTANCE_FLOOR);
        assert!(rec.tags.iter().any(|t| t == REFLECTION_TAG));
        assert!(rec.tags.iter().any(|t| t == "lesson:tool_behavior"));

        // THE loop-closing read: PLAN's lessons selector sees it.
        let lessons = crate::turn::reasoning::inputs::select_lessons(5);
        assert_eq!(lessons, vec!["symbols.find returns dotted ids".to_owned()]);
    }

    #[test]
    fn lesson_learned_twice_reinforces_the_existing_insight() {
        let _env = TestEnv::new();
        std::env::set_var("WYLDE_EMBED_DIM", "3");
        // An existing insight (the first learning) with a vector.
        let first = long_term::save(
            "the indexer skips dotfiles",
            "reflection:turn_critique",
            Some(REFLECTION_IMPORTANCE_FLOOR as f64),
            vec![REFLECTION_TAG.to_owned()],
            Some(vec![1.0, 0.0, 0.0]),
        )
        .unwrap();
        let before = long_term::list_records(true).len();

        // The τ=0.92 dedup half is the shared `duplicate_for_vector`
        // (pinned in memory tests); here we pin what REFLECT does with
        // its verdict: reinforce (touch), never a second record.
        let w = persist_lesson_with(
            &lesson("the indexer ignores dotfiles", 0.9),
            Some(first.id.clone()),
            None,
        );
        assert_eq!(w, LessonWrite::Reinforced(first.id.clone()));
        assert_eq!(
            long_term::list_records(true).len(),
            before,
            "no duplicate record minted"
        );
    }
}
