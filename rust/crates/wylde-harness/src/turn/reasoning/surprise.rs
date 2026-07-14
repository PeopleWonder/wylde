//! Surprise detection + replan-on-surprise (implementation plan §4,
//! slice S4) — **cheap detect, expensive respond**.
//!
//! ## The detection stack (per executed plan step)
//!
//! | layer | mechanism | model cost |
//! |---|---|---|
//! | **L0** | deterministic tool-failure shape: the `run_one_tool` error
//!   envelope (`[error]` / `[tier_blocked]` content prefix) or a
//!   structural error object ([`wylde_reasoning_plan::is_error_envelope`]) | 0 |
//! | **L1** | the step's declared [`ExpectedOutcome`] predicates via the
//!   pure [`wylde_reasoning_plan::evaluate`] | 0 |
//! | **L2** | ONE fast-slot yes/no over the step's `assertion` + a
//!   truncated result digest — fired ONLY when the pure verdict says
//!   `needs_l2` (assertion-only step, or predicates passed at planner
//!   `confidence ≤ 0.75`), at most once per step | 1 cheap call |
//! | **L3** | budget + no-progress: `replans_used >= replan_budget` trips
//!   the visible-note degrade to plain ReAct; a round whose tool calls
//!   were ALL duplicate-suppressed (zero new results with plan steps
//!   still pending) trips replan-or-degrade | 0 |
//!
//! Embedding-distance checks are deliberately absent (rejected in the
//! plan: an embed call per step + an uncalibrated threshold, and L1
//! predicates already encode the expectation more precisely).
//!
//! ## The response (the expensive half)
//!
//! A surprising step consults its planner-declared
//! [`SurpriseAction`]: `continue` logs visibly and proceeds; `abort` ends
//! the turn cleanly (`AbortReason::PlanPrecondition` — the planner marked
//! the step an unrecoverable precondition); `replan` hands
//! {original plan, executed results, surprise verdict} back to the
//! reasoner ([`plan_phase::replan`]) for a REVISED plan, budget-gated by
//! `ReasoningConfig.replan_budget` (default 2). Budget exhaustion
//! degrades to plain ReAct with a visible notice — never a silent stop,
//! never a broken turn.
//!
//! ## L2 call discipline (the tiers-slice lesson applied)
//!
//! Ollama's `num_predict` caps think + content TOGETHER and a generation
//! that dies mid-`<think>` yields ZERO content — so the L2 call sends
//! `think:false` outright (the verdict is a two-field JSON object; there
//! is nothing to deliberate) on a tight [`L2_NUM_PREDICT`] allowance,
//! grammar-constrained by [`l2_verdict_schema`] (gated on the same
//! `constrained_plan` toggle as PLAN). A backend that rejects the think
//! switch gets the standard one-retry-without-it
//! ([`plan_phase::call_reasoner`]).
//!
//! ## Fail-soft (the non-negotiable)
//!
//! Every failure in this module degrades to *continuing*: an L2 call
//! error or unparseable verdict counts as satisfied; a failed replan
//! keeps executing the current plan; budget exhaustion drops to plain
//! ReAct. Nothing here can fail a turn except the planner's own explicit
//! `abort` action — and that is a designed clean end, not a failure.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use wylde_reasoning_plan::{evaluate, is_error_envelope, OutcomePredicate, SurpriseAction};

use crate::config::Config;
use crate::events::{TurnEvent, TurnPhase};
use crate::state::TurnHandle;

use super::config::{Depth, ReasoningConfig};
use super::plan_phase::{self, emit_step};
use super::{ReasoningState, RoundCompletion};

/// `num_predict` for the L2 verdict call — the grammar-guaranteed JSON is
/// two short fields; this bounds a misbehaving backend, not the verdict.
pub const L2_NUM_PREDICT: u32 = 256;

/// Char cap on the result digest handed to the L2 check (~500 tokens —
/// the plan's "digest-truncated" gate on the one cheap call).
pub const L2_DIGEST_MAX_CHARS: usize = 2_000;

/// Char cap on each executed-step result digest in the replan prompt.
pub const REPLAN_DIGEST_MAX_CHARS: usize = 400;

/// What the post-round outcome check tells the driver. Token counts are
/// the extra reasoner/L2 spend this check incurred (folded into the
/// turn's honest meter); `abort` carries the detail of a planner-declared
/// `abort` action — the only path out of here that ends the turn.
#[derive(Debug, Default)]
pub(crate) struct OutcomeFlow {
    pub abort: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// One detected surprise, normalised across L0/L1/L2/no-progress.
struct Surprise {
    step_id: String,
    /// Short cause for the `Step` summary line.
    summary: String,
    /// Specifics (failed checks, digests) for the expandable detail.
    detail: String,
    action: SurpriseAction,
}

/// L0 — deterministic tool-failure detection over the recorded step
/// result. `run_one_tool` renders failures as plain strings prefixed
/// `[error]` / `[tier_blocked]`; some tools return structural error
/// envelopes that survive JSON parsing.
pub(crate) fn is_tool_failure(result: &Value) -> bool {
    match result {
        Value::String(s) => {
            let t = s.trim_start();
            t.starts_with("[error]") || t.starts_with("[tier_blocked]")
        }
        other => is_error_envelope(other),
    }
}

/// Compact, char-boundary-safe digest of a result value for prompts.
pub(crate) fn digest_value(value: &Value, max_chars: usize) -> String {
    let s = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() <= max_chars {
        return s;
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Human-readable one-liner for a failed predicate (the Step detail and
/// the replan prompt speak these, not raw serde).
pub(crate) fn describe_predicate(p: &OutcomePredicate) -> String {
    match p {
        OutcomePredicate::NonEmpty => "non-empty result".to_owned(),
        OutcomePredicate::JsonPathExists { path } => format!("path {path} exists"),
        OutcomePredicate::JsonPathEquals { path, value } => format!("{path} == {value}"),
        OutcomePredicate::Contains { needle, .. } => format!("contains {needle:?}"),
        OutcomePredicate::CountAtLeast { path, n } => format!("{path} has >= {n} item(s)"),
        OutcomePredicate::NoError => "no error".to_owned(),
    }
}

/// The L2 verdict's JSON Schema — the `format` value for the one
/// fast-model yes/no. Tiny and closed: the check may never freehand.
pub fn l2_verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "satisfied": {"type": "boolean"},
            "reason": {"type": "string"}
        },
        "required": ["satisfied", "reason"],
        "additionalProperties": false
    })
}

/// The verdict shape the L2 reply must parse into (lockstep with
/// [`l2_verdict_schema`] — pinned by test).
#[derive(Debug, Deserialize)]
struct L2Verdict {
    satisfied: bool,
    #[serde(default)]
    reason: String,
}

/// The L2 system prompt — stable, per-call content rides the user message.
const L2_SYSTEM_PROMPT: &str = "You are a strict result checker. Reply ONLY with a JSON \
     object {\"satisfied\": true|false, \"reason\": \"one short sentence\"}. Judge whether \
     the actual result satisfies the stated expectation, on the evidence given alone. \
     A result that plainly matches the expectation is satisfied:true; answer \
     satisfied:false only when the result clearly does not deliver what was expected.";

/// One L2 check: a single fast-slot call, grammar-constrained,
/// deliberation off, tight output allowance. `verdict: None` on any
/// failure — the caller treats that as satisfied (fail-soft).
struct L2Outcome {
    verdict: Option<L2Verdict>,
    prompt_tokens: u64,
    completion_tokens: u64,
}

async fn run_l2(
    cfg: &'static Config,
    fast_model: &str,
    intent: &str,
    assertion: &str,
    result: &Value,
) -> L2Outcome {
    let mut options = crate::turn::chat_options::chat_options(fast_model);
    options.insert("num_predict".to_owned(), json!(L2_NUM_PREDICT));
    let messages = json!([
        {"role": "system", "content": L2_SYSTEM_PROMPT},
        {"role": "user", "content": format!(
            "Step intent: {intent}\nExpected: {assertion}\n\
             Actual result (may be truncated):\n{}\n\n\
             Does the actual result satisfy the expectation?",
            digest_value(result, L2_DIGEST_MAX_CHARS)
        )},
    ]);
    // Same toggle as the PLAN grammar (constrained.rs policy table: the
    // L2 verdict is machine-consumed structured output).
    let format = ReasoningConfig::current()
        .constrained_plan
        .then(l2_verdict_schema);

    match plan_phase::call_reasoner(
        cfg,
        fast_model,
        &messages,
        &options,
        Some(false),
        format.as_ref(),
    )
    .await
    {
        Ok(reply) => {
            let prompt_tokens = reply
                .get("prompt_eval_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let completion_tokens = reply.get("eval_count").and_then(Value::as_u64).unwrap_or(0);
            let raw = reply
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let (clean, _think) = plan_phase::strip_inline_think(raw);
            let verdict = serde_json::from_str::<L2Verdict>(clean.trim()).ok();
            if verdict.is_none() {
                tracing::warn!("reasoning: L2 verdict unparseable — treating as satisfied");
            }
            L2Outcome {
                verdict,
                prompt_tokens,
                completion_tokens,
            }
        }
        Err(e) => {
            tracing::warn!(
                "reasoning: L2 check failed ({}: {}) — treating as satisfied",
                e.code,
                e.message
            );
            L2Outcome {
                verdict: None,
                prompt_tokens: 0,
                completion_tokens: 0,
            }
        }
    }
}

/// The post-round outcome seam (seam 3b): detect a surprise on the round
/// that just finished, and respond per the step's declared action. Called
/// by the driver ONLY when a `ReasoningState` exists — the fast path
/// never reaches this module.
#[allow(clippy::too_many_arguments)] // mirrors the turn-driver fan-out
pub(crate) async fn check_and_maybe_replan(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    tier: Depth,
    fast_model: &str,
    alias_map: &HashMap<String, String>,
    state: &mut ReasoningState,
    completion: RoundCompletion,
) -> OutcomeFlow {
    let mut flow = OutcomeFlow::default();
    if state.abandoned {
        return flow;
    }

    let surprise = match detect(cfg, fast_model, state, &completion, &mut flow).await {
        Some(s) => s,
        None => return flow,
    };

    // The S5 critique reads this back: what surprised us is what a
    // lesson is made of.
    state
        .surprise_log
        .push(format!("{}: {}", surprise.step_id, surprise.summary));

    emit_step(
        handle,
        turn_id,
        format!("{} surprised: {}", surprise.step_id, surprise.summary),
        Some(surprise.detail.clone()),
    )
    .await;

    match surprise.action {
        SurpriseAction::Continue => flow,
        SurpriseAction::Abort => {
            flow.abort = Some(format!(
                "plan step {} failed its precondition: {} ({})",
                surprise.step_id, surprise.summary, surprise.detail
            ));
            flow
        }
        SurpriseAction::Replan => {
            respond_with_replan(
                cfg, handle, turn_id, tier, alias_map, state, &surprise, flow,
            )
            .await
        }
    }
}

/// The detect half: L0 → L1 → (gated L2) over the completed step, or the
/// L3 no-progress trip. `None` = nothing surprising (the common case —
/// zero cost beyond the pure checks).
async fn detect(
    cfg: &'static Config,
    fast_model: &str,
    state: &mut ReasoningState,
    completion: &RoundCompletion,
    flow: &mut OutcomeFlow,
) -> Option<Surprise> {
    let step_id = completion.step_id.as_deref()?;

    // L3 no-progress: the round was guided by a step, dispatched tool
    // calls, and produced zero new results (all duplicate-suppressed).
    // Left unhandled this ping-pongs to MAX_TOOL_LOOPS; route it to the
    // replan-or-degrade path directly (the step declared nothing about
    // this failure mode, so its on_surprise doesn't apply).
    if !completion.completed {
        return Some(Surprise {
            step_id: step_id.to_owned(),
            summary: "no progress (all tool calls were duplicates)".to_owned(),
            detail: "the round's tool calls were all duplicate-suppressed; \
                     repeating them cannot advance the plan"
                .to_owned(),
            action: SurpriseAction::Replan,
        });
    }

    let step = state.dag.steps.iter().find(|s| s.id == step_id)?.clone();
    let result = state.results.get(step_id).cloned().unwrap_or(Value::Null);

    // L0 — a failed tool is surprising regardless of declared predicates.
    if is_tool_failure(&result) {
        return Some(Surprise {
            step_id: step_id.to_owned(),
            summary: "the tool returned an error".to_owned(),
            detail: digest_value(&result, REPLAN_DIGEST_MAX_CHARS),
            action: step.expected.on_surprise,
        });
    }

    // L1 — the pure declared-predicate evaluation.
    let verdict = evaluate(&step.expected, &result);
    if verdict.surprised {
        let failed: Vec<String> = verdict
            .failed_predicates
            .iter()
            .map(describe_predicate)
            .collect();
        return Some(Surprise {
            step_id: step_id.to_owned(),
            summary: format!(
                "{} expected check(s) failed",
                verdict.failed_predicates.len()
            ),
            detail: format!(
                "expected: {}; actual: {}",
                failed.join(", "),
                digest_value(&result, REPLAN_DIGEST_MAX_CHARS)
            ),
            action: step.expected.on_surprise,
        });
    }

    // L2 — only when the pure verdict is inconclusive, at most once per
    // step, digest-truncated, deliberation off, grammar-constrained.
    if verdict.needs_l2 && state.l2_checked.insert(step_id.to_owned()) {
        let l2 = run_l2(
            cfg,
            fast_model,
            &step.intent,
            &step.expected.assertion,
            &result,
        )
        .await;
        flow.prompt_tokens += l2.prompt_tokens;
        flow.completion_tokens += l2.completion_tokens;
        if let Some(v) = l2.verdict {
            if !v.satisfied {
                let reason = if v.reason.trim().is_empty() {
                    "the checker judged the result unsatisfying".to_owned()
                } else {
                    v.reason
                };
                return Some(Surprise {
                    step_id: step_id.to_owned(),
                    summary: "outcome check said no".to_owned(),
                    detail: format!(
                        "expected: {}; checker: {reason}; actual: {}",
                        step.expected.assertion,
                        digest_value(&result, REPLAN_DIGEST_MAX_CHARS)
                    ),
                    action: step.expected.on_surprise,
                });
            }
        }
    }

    None
}

/// The respond half for `SurpriseAction::Replan`: budget gate → visible
/// `Replanning` phase → one reasoner call → adopt the revision (or keep
/// the current plan on failure). Exhaustion degrades to plain ReAct with
/// a visible notice.
#[allow(clippy::too_many_arguments)] // mirrors the turn-driver fan-out
async fn respond_with_replan(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    tier: Depth,
    alias_map: &HashMap<String, String>,
    state: &mut ReasoningState,
    surprise: &Surprise,
    mut flow: OutcomeFlow,
) -> OutcomeFlow {
    let budget = ReasoningConfig::current().replan_budget;
    if state.replans_used >= budget {
        emit_step(
            handle,
            turn_id,
            format!("Replan budget exhausted ({budget}) — continuing without the plan"),
            Some(format!(
                "{} surprised again after {budget} replan(s); the rest of the \
                 turn runs as plain ReAct",
                surprise.step_id
            )),
        )
        .await;
        state.abandoned = true;
        return flow;
    }
    state.replans_used += 1;

    handle
        .push_turn_event(TurnEvent::Phase {
            turn_id: turn_id.to_owned(),
            phase: TurnPhase::Replanning,
        })
        .await;
    emit_step(
        handle,
        turn_id,
        format!("Replanning ({} of {budget})…", state.replans_used),
        Some(format!(
            "{}: {} — handing the surprise back to the reasoner",
            surprise.step_id, surprise.summary
        )),
    )
    .await;

    let surprise_text = format!(
        "Step {} ({}) surprised: {}.\nDetail: {}",
        surprise.step_id,
        state
            .dag
            .steps
            .iter()
            .find(|s| s.id == surprise.step_id)
            .and_then(|s| s.tool.as_deref())
            .unwrap_or("no tool"),
        surprise.summary,
        surprise.detail
    );
    let executed = state.executed_log.clone();
    let revised = plan_phase::replan(
        cfg,
        handle,
        turn_id,
        tier,
        &state.plan_inputs,
        &state.dag,
        &executed,
        &surprise_text,
        alias_map,
    )
    .await;

    let Some(call) = revised else {
        // `replan` already emitted its visible failure notice; keep
        // executing the current plan (fail-soft).
        return flow;
    };
    flow.prompt_tokens += call.prompt_tokens;
    flow.completion_tokens += call.completion_tokens;

    if call.dag.steps.is_empty() {
        emit_step(
            handle,
            turn_id,
            format!(
                "Plan revised (v{}): answer directly (0 steps)",
                call.dag.plan_version
            ),
            None,
        )
        .await;
    } else {
        for (i, step) in call.dag.steps.iter().enumerate() {
            emit_step(
                handle,
                turn_id,
                format!("{} · {}", step.id, step.intent),
                Some(plan_phase::step_detail(step, i)),
            )
            .await;
        }
        emit_step(
            handle,
            turn_id,
            format!(
                "Plan revised (v{}): {} step(s) in {:.1}s",
                call.dag.plan_version,
                call.dag.steps.len(),
                call.elapsed_ms as f64 / 1000.0
            ),
            Some(format!(
                "reasoner {} prompt + {} completion tokens",
                call.prompt_tokens, call.completion_tokens
            )),
        )
        .await;
    }
    state.adopt_revised_plan(call.dag);
    flow
}

#[cfg(test)]
mod tests {
    use super::*;
    use wylde_reasoning_plan::{ExpectedOutcome, PlanDag, PlanStep};

    #[test]
    fn l0_catches_the_tool_error_shapes() {
        // The run_one_tool string envelopes.
        assert!(is_tool_failure(&json!(
            "[error] ollama_unreachable: connect refused"
        )));
        assert!(is_tool_failure(&json!("[tier_blocked] tool blocked")));
        // Structural envelopes that survive JSON parsing.
        assert!(is_tool_failure(&json!({"error": "boom"})));
        assert!(is_tool_failure(&json!({"ok": false})));
        assert!(is_tool_failure(&json!({"status": "ERROR"})));
        // Clean results do not trip.
        assert!(!is_tool_failure(&json!("2026-07-13T12:00:00Z")));
        assert!(!is_tool_failure(&json!({"entries": ["a"]})));
        assert!(!is_tool_failure(&json!({"error": null, "ok": true})));
        assert!(
            !is_tool_failure(&Value::Null),
            "null is empty, not an error"
        );
    }

    #[test]
    fn digest_truncates_on_char_boundaries() {
        assert_eq!(digest_value(&json!("short"), 10), "short");
        // Multi-byte chars: truncation must never split a code point.
        let s = "é".repeat(20);
        let d = digest_value(&json!(s), 5);
        assert_eq!(d, format!("{}…", "é".repeat(5)));
        // Non-string values digest as compact JSON.
        assert_eq!(digest_value(&json!({"a": 1}), 100), "{\"a\":1}");
    }

    #[test]
    fn predicate_descriptions_are_human() {
        assert_eq!(
            describe_predicate(&OutcomePredicate::CountAtLeast {
                path: "/entries".into(),
                n: 1
            }),
            "/entries has >= 1 item(s)"
        );
        assert_eq!(
            describe_predicate(&OutcomePredicate::NonEmpty),
            "non-empty result"
        );
    }

    #[test]
    fn l2_schema_and_verdict_stay_lockstep() {
        // Direction 1: a schema-minimal object deserializes.
        let v: L2Verdict =
            serde_json::from_str("{\"satisfied\": true, \"reason\": \"ok\"}").unwrap();
        assert!(v.satisfied);
        assert_eq!(v.reason, "ok");
        // Tolerance: a missing reason still parses (grammar requires it,
        // but a freehand-degraded backend may drop it).
        let v: L2Verdict = serde_json::from_str("{\"satisfied\": false}").unwrap();
        assert!(!v.satisfied);
        assert!(v.reason.is_empty());
        // Direction 2: the schema requires exactly the struct's fields.
        let schema = l2_verdict_schema();
        let req: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(req, ["satisfied", "reason"]);
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn replan_prompt_renders_the_full_surprise_context() {
        let inputs = super::super::inputs::PlanInputs {
            goal: "find the config loader".into(),
            exclusions: vec!["auth-flow IS NOT oauth-shim — different subsystem".into()],
            tool_catalog: vec!["workspaces.rag_query — search the workspace".into()],
            ..Default::default()
        };
        let dag = PlanDag {
            goal: "g".into(),
            steps: vec![
                PlanStep {
                    id: "s1".into(),
                    intent: "search".into(),
                    tool: Some("workspaces.rag_query".into()),
                    args_template: json!({}),
                    depends_on: vec![],
                    expected: ExpectedOutcome::trusting(),
                },
                PlanStep {
                    id: "s2".into(),
                    intent: "read it".into(),
                    tool: None,
                    args_template: json!({}),
                    depends_on: vec!["s1".into()],
                    expected: ExpectedOutcome::trusting(),
                },
            ],
            reasoning_trace: String::new(),
            plan_version: 1,
        };
        let executed = vec![super::super::ExecutedStep {
            id: "s1".into(),
            tool: Some("workspaces.rag_query".into()),
            digest: "{\"matches\":[]}".into(),
        }];
        let p = plan_phase::render_replan_prompt(
            &inputs,
            &dag,
            &executed,
            "Step s1 surprised: 1 expected check(s) failed",
        );
        assert!(p.starts_with("### Goal\nfind the config loader"), "{p}");
        assert!(p.contains("### Excluded — NOT relevant"), "{p}");
        assert!(p.contains("### Available tools"), "{p}");
        assert!(p.contains("### Plan under revision (version 1)"), "{p}");
        assert!(
            p.contains("s1 · search · workspaces.rag_query · done"),
            "{p}"
        );
        assert!(
            p.contains("s2 · read it · synthesis (no tool) · pending"),
            "{p}"
        );
        assert!(p.contains("### Executed step results"), "{p}");
        assert!(
            p.contains("s1 (workspaces.rag_query) → {\"matches\":[]}"),
            "{p}"
        );
        assert!(p.contains("### Surprise\nStep s1 surprised"), "{p}");
        assert!(p.contains("FRESH step ids"), "{p}");
    }

    #[test]
    fn replan_prompt_omits_empty_sections() {
        let inputs = super::super::inputs::PlanInputs {
            goal: "g".into(),
            ..Default::default()
        };
        let dag = PlanDag {
            goal: "g".into(),
            steps: vec![],
            reasoning_trace: String::new(),
            plan_version: 2,
        };
        let p = plan_phase::render_replan_prompt(&inputs, &dag, &[], "why");
        assert!(!p.contains("### Excluded"), "{p}");
        assert!(!p.contains("### Available tools"), "{p}");
        assert!(!p.contains("### Executed step results"), "{p}");
        assert!(p.contains("### Surprise\nwhy"), "{p}");
    }
}
