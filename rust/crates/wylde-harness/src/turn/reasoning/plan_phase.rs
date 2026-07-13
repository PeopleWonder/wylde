//! The PLAN phase — one unary reasoner call → a validated
//! [`PlanDag`] (implementation plan §3.3, slice S3).
//!
//! **Fail-soft is the contract**: every failure path (unreachable
//! backend, malformed JSON, unknown tool names, cyclic deps) emits a
//! VISIBLE `Step(Reasoning)` notice and returns `None` — the caller falls
//! back to plain ReAct and the turn proceeds. A bad plan costs one wasted
//! reasoner call, never a broken turn.
//!
//! The call goes through
//! [`constrained::ollama_chat_maybe_constrained`] with
//! [`constrained::plan_format`] (S1.5 — grammar-constrained decoding,
//! eval-backed 93.3% → 100% schema-valid on the default reasoner) and the
//! reasoner's `num_ctx` is capped at [`REASONER_NUM_CTX_CAP`] — the
//! 35B-A3B quant's measured fully-GPU-resident ceiling (spills ≥65k).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use wylde_reasoning_plan::{PlanDag, PlanStep};

use crate::config::Config;
use crate::events::{StepStage, TurnEvent, TurnPhase};
use crate::state::TurnHandle;
use crate::turn::context_gather::GatheredContext;
use crate::turn::think_stream::ThinkSplitter;

use super::config::{Depth, ReasoningConfig};
use super::{constrained, inputs};

/// Hard cap on plan length — mirrors `tool_round::MAX_TOOL_LOOPS` so a
/// full plan can actually execute inside the loop's round budget (R6: the
/// loop cap stays authoritative).
pub const MAX_PLAN_STEPS: usize = crate::turn::tool_round::MAX_TOOL_LOOPS;

/// The reasoner call's `num_ctx` ceiling. The default reasoner
/// (Qwen3.6-35B-A3B UD-IQ3_XXS) is 100% GPU-resident at 32k and spills at
/// 65k+ (S1.5 eval) — a user override may shrink this, never grow it.
pub const REASONER_NUM_CTX_CAP: u64 = 32_768;

/// Output allowance ON TOP of the tier's think budget for the plan JSON
/// itself. Ollama's `num_predict` caps think + content TOGETHER (there is
/// no separate think cap), and the S3 live measurement showed the default
/// reasoner ruminating ~3.5–4k tokens on a grounded plan prompt — a bare
/// `num_predict = think_budget (4096)` truncated the JSON mid-string on
/// 1 of 2 warm calls. Every tier carries this headroom on top of its
/// think allowance; the `Think` tier (deliberation off) runs on this
/// allowance alone. The sum still hard-bounds a meltdown.
pub const PLAN_OUTPUT_BUDGET: u32 = 2_048;

/// What one successful PLAN call produced — the validated DAG plus the
/// honest cost numbers the driver folds into the turn's token meter.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanOutcome {
    pub dag: PlanDag,
    /// Ollama `prompt_eval_count` for the plan call (0 when omitted).
    pub prompt_tokens: u64,
    /// Ollama `eval_count` for the plan call (0 when omitted).
    pub completion_tokens: u64,
    /// Wall-clock of the reasoner call itself.
    pub elapsed_ms: u64,
    /// The grounded inputs the plan was prompted with — retained on the
    /// turn's `ReasoningState` so a replan (S4) can re-render the goal,
    /// exclusions and tool catalog without re-gathering (no second IPC
    /// fan-out on the surprise path).
    pub plan_inputs: inputs::PlanInputs,
}

/// One reasoner→DAG call's result — shared by PLAN and REPLAN (S4). The
/// caller owns the phase-specific emissions (checklist vs revision
/// notice); this carries only the artifact + the honest cost numbers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DagCall {
    pub dag: PlanDag,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub elapsed_ms: u64,
}

/// Why a reasoner→DAG call yielded no plan. `Cancelled` is silent (the
/// driver's round boundary owns the abort); the other two carry the
/// human-readable detail for the caller's visible fallback notice.
#[derive(Debug)]
pub(crate) enum DagCallError {
    Cancelled,
    /// The IPC call itself failed — `"{code}: {message}"`.
    Unavailable(String),
    /// The reply parsed/validated to nothing usable.
    Invalid(String),
}

/// Run the PLAN phase at the turn's tier. Emits `Phase(Planning)`, the
/// grounding step, the reasoner's thinking, then either the per-step plan
/// checklist or a visible fallback notice. `None` ⇒ run plain ReAct (the
/// caller changes nothing else).
///
/// `failures` is empty on an ordinary Deep turn; on an S4b auto-escalated
/// Fast turn it carries the hard-tool-failure digests that triggered the
/// escalation, rendered as their own prompt section so the planner routes
/// around them.
#[allow(clippy::too_many_arguments)] // mirrors the driver fan-out it's called from
pub(crate) async fn run(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    depth: Depth,
    workspace_id: Option<&str>,
    user_message: &str,
    gathered: &GatheredContext,
    alias_map: &HashMap<String, String>,
    failures: &[String],
) -> Option<PlanOutcome> {
    handle
        .push_turn_event(TurnEvent::Phase {
            turn_id: turn_id.to_owned(),
            phase: TurnPhase::Planning,
        })
        .await;

    let mut plan_inputs = inputs::gather(workspace_id, user_message, gathered).await;
    plan_inputs.failures = failures.to_vec();
    let grounding_detail = gathered.route_candidates.as_ref().and_then(|set| {
        let names: Vec<String> = set.activated().map(|c| c.label.clone()).collect();
        (!names.is_empty()).then(|| names.join(", "))
    });
    emit_step(
        handle,
        turn_id,
        plan_inputs.grounding_summary(),
        grounding_detail,
    )
    .await;

    if handle.is_cancelled() {
        return None;
    }

    let messages = json!([
        {"role": "system", "content": inputs::plan_system_prompt()},
        {"role": "user", "content": inputs::render_user_prompt(&plan_inputs)},
    ]);
    let DagCall {
        dag,
        prompt_tokens,
        completion_tokens,
        elapsed_ms,
    } = match tiered_dag_call(cfg, handle, turn_id, depth, &messages, alias_map).await {
        Ok(call) => call,
        Err(DagCallError::Cancelled) => return None,
        Err(DagCallError::Unavailable(detail)) => {
            tracing::warn!("reasoning: PLAN call failed ({detail})");
            emit_step(
                handle,
                turn_id,
                "Planner unavailable — running direct",
                Some(detail),
            )
            .await;
            return None;
        }
        Err(DagCallError::Invalid(reason)) => {
            tracing::warn!("reasoning: PLAN parse/validation failed: {reason}");
            emit_step(
                handle,
                turn_id,
                "Planner output invalid — running direct",
                Some(reason),
            )
            .await;
            return None;
        }
    };

    if dag.steps.is_empty() {
        // A legal "answer directly" plan — fall through to plain ReAct
        // with the trace kept (plan §2).
        emit_step(handle, turn_id, "Plan: answer directly (0 steps)", None).await;
    } else {
        // THE plan checklist: one Step(Reasoning) per plan step — rendered
        // by the existing grouped activity dropdown, zero new widgetry.
        for (i, step) in dag.steps.iter().enumerate() {
            emit_step(
                handle,
                turn_id,
                format!("{} · {}", step.id, step.intent),
                Some(step_detail(step, i)),
            )
            .await;
        }
        emit_step(
            handle,
            turn_id,
            format!(
                "Plan drafted: {} step(s) in {:.1}s",
                dag.steps.len(),
                elapsed_ms as f64 / 1000.0
            ),
            Some(format!(
                "reasoner {prompt_tokens} prompt + {completion_tokens} completion tokens"
            )),
        )
        .await;
    }

    Some(PlanOutcome {
        dag,
        prompt_tokens,
        completion_tokens,
        elapsed_ms,
        plan_inputs,
    })
}

/// The shared reasoner→DAG core (PLAN and REPLAN, S4): tier knobs
/// (`num_ctx` cap, think budget + [`PLAN_OUTPUT_BUDGET`], the tier's
/// `think` switch), the grammar-constrained call, thinking-event
/// emission, the think-exhaustion salvage retry, and the parse/validation
/// ladder. Behaviour is exactly the S3/tiers PLAN path — this is a
/// factoring, not a change; the S3 e2e suite pins the wire bodies.
async fn tiered_dag_call(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    depth: Depth,
    messages: &Value,
    alias_map: &HashMap<String, String>,
) -> Result<DagCall, DagCallError> {
    let rcfg = ReasoningConfig::current();
    let model = rcfg.slots.reasoner.clone();

    // Per-model user overrides ride the call, with two reasoning-owned
    // knobs on top: the generation cap (R2's `num_predict` guard — the
    // grammar can't stop rumination, this can; the TIER's think budget +
    // the JSON output allowance, see [`PLAN_OUTPUT_BUDGET`]) and the
    // resident-ctx cap.
    let mut options = crate::turn::chat_options::chat_options(&model);
    let num_ctx = options
        .get("num_ctx")
        .and_then(Value::as_u64)
        .map(|v| v.min(REASONER_NUM_CTX_CAP))
        .unwrap_or(REASONER_NUM_CTX_CAP);
    options.insert("num_ctx".to_owned(), json!(num_ctx));
    let think_budget = depth.think_budget(&rcfg.tier_budgets);
    options.insert(
        "num_predict".to_owned(),
        json!(think_budget.saturating_add(PLAN_OUTPUT_BUDGET)),
    );

    // The tier's think switch: the `Think` tier sends `think:false`
    // (plan grammar-first, no rumination — measured ~2–6 s vs tens of
    // seconds); the deliberating tiers OMIT the field so a non-thinking
    // reasoner keeps working exactly as in S3 (Ollama rejects an explicit
    // `think` on models without the capability).
    let think = (!depth.think_enabled()).then_some(false);

    let format = constrained::plan_format();
    let started = Instant::now();
    let upstream =
        match call_reasoner(cfg, &model, messages, &options, think, format.as_ref()).await {
            Ok(v) => v,
            Err(e) => {
                return Err(DagCallError::Unavailable(format!(
                    "{}: {}",
                    e.code, e.message
                )))
            }
        };

    if handle.is_cancelled() {
        return Err(DagCallError::Cancelled);
    }

    let mut prompt_tokens = upstream
        .get("prompt_eval_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut completion_tokens = upstream
        .get("eval_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Surface the reasoner's thinking: the native `message.thinking` field
    // (thinking-API models) and any inline `<think>` block peeled off the
    // content body both feed the existing Thinking dropdown.
    let mut emitted_thinking = false;
    if let Some(t) = upstream
        .get("message")
        .and_then(|m| m.get("thinking"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        emit_thinking(handle, turn_id, t).await;
        emitted_thinking = true;
    }
    let raw_content = upstream
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let (mut clean_content, inline_think) = strip_inline_think(raw_content);
    if !inline_think.trim().is_empty() {
        emit_thinking(handle, turn_id, &inline_think).await;
        emitted_thinking = true;
    }

    // Think-exhaustion salvage (tiers slice): a deliberating tier whose
    // generation hit the num_predict cap while still inside `<think>`
    // produced ZERO content — the grammar constrains `message.content`
    // only and cannot force the model out of the think channel (measured:
    // 2 of 3 seeds at the old 4096+2048 default died exactly this way,
    // ~37 s wasted then plain ReAct). One grammar-first retry with
    // deliberation disabled turns that into a valid — if unthought — plan
    // for ~2–6 s more. Bounded: one retry, output allowance only.
    let exhausted = depth.think_enabled()
        && clean_content.trim().is_empty()
        && upstream.get("done_reason").and_then(Value::as_str) == Some("length");
    if exhausted {
        emit_step(
            handle,
            turn_id,
            "Deliberation used the whole budget — retrying without it",
            Some(format!(
                "{think_budget} think tokens exhausted before any plan JSON; \
                 one grammar-first retry (think off)"
            )),
        )
        .await;
        let mut salvage_options = options.clone();
        salvage_options.insert("num_predict".to_owned(), json!(PLAN_OUTPUT_BUDGET));
        match call_reasoner(
            cfg,
            &model,
            messages,
            &salvage_options,
            Some(false),
            format.as_ref(),
        )
        .await
        {
            Ok(v) => {
                prompt_tokens += v
                    .get("prompt_eval_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                completion_tokens += v.get("eval_count").and_then(Value::as_u64).unwrap_or(0);
                let raw = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                clean_content = strip_inline_think(raw).0;
            }
            Err(e) => {
                tracing::warn!(
                    "reasoning: think-exhaustion salvage failed ({}: {})",
                    e.code,
                    e.message
                );
            }
        }
        if handle.is_cancelled() {
            return Err(DagCallError::Cancelled);
        }
    }

    let dag = parse_plan_dag(&clean_content, alias_map).map_err(DagCallError::Invalid)?;
    let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    // The JSON-carried trace reaches the dropdown too when nothing else
    // did (a non-thinking backend still explains its plan).
    if !emitted_thinking && !dag.reasoning_trace.trim().is_empty() {
        emit_thinking(handle, turn_id, &dag.reasoning_trace).await;
    }

    Ok(DagCall {
        dag,
        prompt_tokens,
        completion_tokens,
        elapsed_ms,
    })
}

/// REPLAN (S4, the expensive half of replan-on-surprise): one reasoner
/// call handing back the original plan, the executed results, and the
/// surprise verdict — out comes a REVISED [`PlanDag`] for the remaining
/// work, with `plan_version` bumped past the current plan's.
///
/// Same tier knobs, grammar constraint, think-exhaustion salvage and
/// validation as PLAN (via [`tiered_dag_call`]). **Fail-soft**: any
/// failure emits a visible "Replan failed — continuing the existing plan"
/// notice and returns `None`; the caller keeps executing the current
/// plan. Cancellation is silent (the round boundary owns the abort).
#[allow(clippy::too_many_arguments)] // mirrors the surprise-seam fan-out
pub(crate) async fn replan(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    depth: Depth,
    plan_inputs: &inputs::PlanInputs,
    dag: &PlanDag,
    executed: &[super::ExecutedStep],
    surprise: &str,
    alias_map: &HashMap<String, String>,
) -> Option<DagCall> {
    let messages = json!([
        {"role": "system", "content": inputs::plan_system_prompt()},
        {"role": "user", "content": render_replan_prompt(plan_inputs, dag, executed, surprise)},
    ]);
    match tiered_dag_call(cfg, handle, turn_id, depth, &messages, alias_map).await {
        Ok(mut call) => {
            // `validate_plan` normalises to 1; a revision supersedes the
            // plan it revised.
            call.dag.plan_version = dag.plan_version + 1;
            Some(call)
        }
        Err(DagCallError::Cancelled) => None,
        Err(DagCallError::Unavailable(detail)) | Err(DagCallError::Invalid(detail)) => {
            tracing::warn!("reasoning: REPLAN failed: {detail}");
            emit_step(
                handle,
                turn_id,
                "Replan failed — continuing the existing plan",
                Some(detail),
            )
            .await;
            None
        }
    }
}

/// Render the REPLAN call's user message. Pure string assembly
/// (golden-testable). Reuses the retained [`inputs::PlanInputs`] for the
/// goal, the IS-NOT exclusions and the tool catalog — the surfaces a
/// revision must still respect — and adds the plan under revision, the
/// executed results, and the surprise verdict. Live concepts/boundaries/
/// digest are deliberately dropped: the executed evidence is now the
/// sharper grounding, and the replan prompt should stay lean.
pub(crate) fn render_replan_prompt(
    inputs: &inputs::PlanInputs,
    dag: &PlanDag,
    executed: &[super::ExecutedStep],
    surprise: &str,
) -> String {
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
        s.truncate(s.trim_end().len());
    };

    section(
        "Excluded — NOT relevant (user-authored boundaries)",
        &inputs.exclusions,
        &mut s,
    );
    section("Available tools", &inputs.tool_catalog, &mut s);

    let plan_lines: Vec<String> = dag
        .steps
        .iter()
        .map(|step| {
            let tool = step.tool.as_deref().unwrap_or("synthesis (no tool)");
            let done = if executed.iter().any(|e| e.id == step.id) {
                " · done"
            } else {
                " · pending"
            };
            format!("{} · {} · {tool}{done}", step.id, step.intent)
        })
        .collect();
    section(
        &format!("Plan under revision (version {})", dag.plan_version),
        &plan_lines,
        &mut s,
    );

    let executed_lines: Vec<String> = executed
        .iter()
        .map(|e| {
            let tool = e.tool.as_deref().unwrap_or("synthesis");
            format!("{} ({tool}) → {}", e.id, e.digest)
        })
        .collect();
    section("Executed step results", &executed_lines, &mut s);

    s.push_str("\n\n### Surprise\n");
    s.push_str(surprise);

    s.push_str(
        "\n\n### Instructions\n\
         The plan above hit an unexpected result. Produce a REVISED complete \
         plan JSON (same schema) covering ONLY the remaining work toward the \
         goal.\n\
         - Executed step results stay available: reference them with \
         ${stepid.output} / ${stepid.output.field.path}.\n\
         - Use FRESH step ids (e.g. \"r1\", \"r2\") that do not collide with \
         executed step ids, and never depends_on an executed step.\n\
         - Route around the surprise; do not re-issue the exact tool call \
         that surprised.\n\
         - If the goal can now be answered directly from the results so far \
         (or cannot be advanced at all), return \"steps\": [].",
    );
    s
}

/// One unary reasoner call: builds the `ollama.chat` body (system + user,
/// non-streamed, 24h keep_alive), attaches the tier's `think` switch when
/// given, and rides the constrained wrapper (grammar + fail-soft format
/// retry). A backend that rejects the `think` field (Ollama errors on an
/// explicit `think` for models without the thinking capability) gets ONE
/// retry without the field — think-off is such a model's only behaviour
/// anyway, so dropping the switch is behaviour-preserving.
pub(crate) async fn call_reasoner(
    cfg: &'static Config,
    model: &str,
    messages: &Value,
    options: &serde_json::Map<String, Value>,
    think: Option<bool>,
    format: Option<&Value>,
) -> Result<Value, wylde_shared::ipc::IpcError> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "priority": cfg.default_chat_priority,
        "stream": false,
        "keep_alive": "24h",
        "options": Value::Object(options.clone()),
    });
    if let Some(t) = think {
        body["think"] = json!(t);
    }
    match constrained::ollama_chat_maybe_constrained(&cfg.ollama_service, body.clone(), format)
        .await
    {
        Err(e) if think.is_some() && e.code == "ollama_http" => {
            tracing::warn!(
                "reasoning: backend rejected the think switch ({}: {}); retrying without it",
                e.code,
                e.message
            );
            if let Some(obj) = body.as_object_mut() {
                obj.remove("think");
            }
            constrained::ollama_chat_maybe_constrained(&cfg.ollama_service, body, format).await
        }
        other => other,
    }
}

/// The checklist row's supporting detail: tool + declared expectation.
pub(crate) fn step_detail(step: &PlanStep, index: usize) -> String {
    let tool = step
        .tool
        .clone()
        .unwrap_or_else(|| "synthesis (no tool)".to_owned());
    let expectation = if !step.expected.predicates.is_empty() {
        format!("expects {} check(s)", step.expected.predicates.len())
    } else if !step.expected.assertion.trim().is_empty() {
        format!("expects: {}", step.expected.assertion.trim())
    } else {
        "no declared expectation".to_owned()
    };
    let deps = if step.depends_on.is_empty() {
        String::new()
    } else {
        format!(" · after {}", step.depends_on.join(", "))
    };
    format!("step {} · {tool} · {expectation}{deps}", index + 1)
}

pub(crate) async fn emit_step(
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    summary: impl Into<String>,
    detail: Option<String>,
) {
    handle
        .push_turn_event(TurnEvent::Step {
            turn_id: turn_id.to_owned(),
            stage: StepStage::Reasoning,
            summary: summary.into(),
            detail,
        })
        .await;
}

async fn emit_thinking(handle: &Arc<TurnHandle>, turn_id: &str, text: &str) {
    handle
        .push_turn_event(TurnEvent::Thinking {
            turn_id: turn_id.to_owned(),
            text: text.to_owned(),
        })
        .await;
}

/// Peel an inline `<think>…</think>` block off a UNARY body by driving
/// the streaming splitter over it in one push (identity on think-free
/// text — the splitter's tested invariant).
pub(crate) fn strip_inline_think(body: &str) -> (String, String) {
    let mut splitter = ThinkSplitter::new();
    let mut delta = splitter.push(body);
    let tail = splitter.finish();
    delta.answer.push_str(&tail.answer);
    delta.thinking.push_str(&tail.thinking);
    (delta.answer, delta.thinking)
}

// ── parse + validation (pure — unit-tested without a pipe) ──────────────

/// Parse the reasoner's (think-stripped) body into a validated
/// [`PlanDag`]. Recovery ladder: direct parse → fenced ```json``` block →
/// first balanced `{…}` object. Then structural validation +
/// tool-name canonicalisation. Every `Err` is a human-readable reason for
/// the fallback notice.
pub fn parse_plan_dag(text: &str, alias_map: &HashMap<String, String>) -> Result<PlanDag, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("planner returned an empty body".to_owned());
    }

    let mut candidates: Vec<String> = Vec::new();
    if trimmed.starts_with('{') {
        candidates.push(trimmed.to_owned());
    }
    candidates.extend(fenced_json_blocks(trimmed));
    if let Some(b) = first_balanced_object(trimmed) {
        candidates.push(b);
    }

    let mut last_err = "no JSON object found in planner output".to_owned();
    for c in candidates {
        match serde_json::from_str::<PlanDag>(&c) {
            Ok(dag) => return validate_plan(dag, alias_map),
            Err(e) => last_err = format!("plan JSON did not match the schema: {e}"),
        }
    }
    Err(last_err)
}

/// Every ```json-fenced block body that looks like an object.
fn fenced_json_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        match body.find("```") {
            Some(close) => {
                let inner = body[..close].trim();
                if inner.starts_with('{') {
                    out.push(inner.to_owned());
                }
                rest = &body[close + 3..];
            }
            None => break,
        }
    }
    out
}

/// The first balanced top-level `{…}` in `text`, string/escape-aware.
fn first_balanced_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

/// Structural validation + canonicalisation:
/// * ≤ [`MAX_PLAN_STEPS`] steps, unique non-empty ids;
/// * every named `tool` resolves through the executor's alias map (and is
///   rewritten to its canonical id so guidance names the real verb);
/// * `depends_on` edges reference existing steps, no self-edges, acyclic;
/// * `plan_version` normalised to 1 (replans bump it — S4).
pub fn validate_plan(
    mut dag: PlanDag,
    alias_map: &HashMap<String, String>,
) -> Result<PlanDag, String> {
    if dag.steps.len() > MAX_PLAN_STEPS {
        return Err(format!(
            "plan has {} steps (max {MAX_PLAN_STEPS})",
            dag.steps.len()
        ));
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for step in &dag.steps {
        if step.id.trim().is_empty() {
            return Err("a plan step has an empty id".to_owned());
        }
        if !seen.insert(step.id.as_str()) {
            return Err(format!("duplicate step id {:?}", step.id));
        }
    }

    let ids: HashSet<String> = dag.steps.iter().map(|s| s.id.clone()).collect();
    for step in &mut dag.steps {
        if let Some(tool) = &step.tool {
            let canonical = alias_map
                .get(tool)
                .cloned()
                .or_else(|| alias_map.values().any(|v| v == tool).then(|| tool.clone()));
            match canonical {
                Some(c) => step.tool = Some(c),
                None => return Err(format!("step {:?} names unknown tool {:?}", step.id, tool)),
            }
        }
        for dep in &step.depends_on {
            if dep == &step.id {
                return Err(format!("step {:?} depends on itself", step.id));
            }
            if !ids.contains(dep) {
                return Err(format!(
                    "step {:?} depends on unknown step {:?}",
                    step.id, dep
                ));
            }
        }
    }

    if has_cycle(&dag.steps) {
        return Err("plan dependency graph has a cycle".to_owned());
    }

    dag.plan_version = 1;
    Ok(dag)
}

/// Kahn's algorithm over the `depends_on` edges.
fn has_cycle(steps: &[PlanStep]) -> bool {
    let index: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let mut indegree = vec![0usize; steps.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); steps.len()];
    for (i, s) in steps.iter().enumerate() {
        for dep in &s.depends_on {
            if let Some(&d) = index.get(dep.as_str()) {
                indegree[i] += 1;
                dependents[d].push(i);
            }
        }
    }
    let mut queue: Vec<usize> = indegree
        .iter()
        .enumerate()
        .filter(|(_, &d)| d == 0)
        .map(|(i, _)| i)
        .collect();
    let mut visited = 0usize;
    while let Some(n) = queue.pop() {
        visited += 1;
        for &m in &dependents[n] {
            indegree[m] -= 1;
            if indegree[m] == 0 {
                queue.push(m);
            }
        }
    }
    visited != steps.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wylde_reasoning_plan::{ExpectedOutcome, OutcomePredicate, SurpriseAction};

    fn aliases() -> HashMap<String, String> {
        HashMap::from([
            (
                "symbols.find".to_owned(),
                "workspaces.symbols.find".to_owned(),
            ),
            (
                "workspaces.symbols.find".to_owned(),
                "workspaces.symbols.find".to_owned(),
            ),
            (
                "workspaces.rag_query".to_owned(),
                "workspaces.rag_query".to_owned(),
            ),
        ])
    }

    fn plan_json(steps: &str) -> String {
        format!(
            "{{\"goal\": \"g\", \"steps\": {steps}, \
             \"reasoning_trace\": \"because\", \"plan_version\": 3}}"
        )
    }

    fn one_step(id: &str, tool: &str, deps: &str) -> String {
        format!(
            "{{\"id\": \"{id}\", \"intent\": \"do {id}\", \"tool\": \"{tool}\", \
             \"args_template\": {{}}, \"depends_on\": {deps}, \
             \"expected\": {{\"predicates\": [], \"assertion\": \"\", \
             \"on_surprise\": \"continue\", \"confidence\": 1.0}}}}"
        )
    }

    #[test]
    fn direct_json_parses_and_normalises_version() {
        let text = plan_json(&format!(
            "[{}]",
            one_step("s1", "workspaces.rag_query", "[]")
        ));
        let dag = parse_plan_dag(&text, &aliases()).expect("valid plan");
        assert_eq!(dag.steps.len(), 1);
        assert_eq!(dag.plan_version, 1, "first plan is always version 1");
    }

    #[test]
    fn think_block_is_stripped_before_parse() {
        let text = format!(
            "<think>let me plan this out…</think>\n{}",
            plan_json(&format!(
                "[{}]",
                one_step("s1", "workspaces.rag_query", "[]")
            ))
        );
        let (clean, think) = strip_inline_think(&text);
        assert_eq!(think, "let me plan this out…");
        parse_plan_dag(&clean, &aliases()).expect("post-think body parses");
    }

    #[test]
    fn fenced_and_prose_wrapped_json_recovers() {
        let inner = plan_json(&format!(
            "[{}]",
            one_step("s1", "workspaces.rag_query", "[]")
        ));
        let fenced = format!("Here is the plan:\n```json\n{inner}\n```\nDone.");
        parse_plan_dag(&fenced, &aliases()).expect("fenced recovers");
        let prose = format!("The plan follows. {inner} That is all.");
        parse_plan_dag(&prose, &aliases()).expect("balanced-brace recovers");
    }

    #[test]
    fn alias_names_canonicalise() {
        let text = plan_json(&format!("[{}]", one_step("s1", "symbols.find", "[]")));
        let dag = parse_plan_dag(&text, &aliases()).unwrap();
        assert_eq!(
            dag.steps[0].tool.as_deref(),
            Some("workspaces.symbols.find")
        );
    }

    #[test]
    fn unknown_tool_fails_validation() {
        let text = plan_json(&format!("[{}]", one_step("s1", "fs.rm_rf", "[]")));
        let err = parse_plan_dag(&text, &aliases()).unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[test]
    fn null_tool_synthesis_step_is_legal() {
        let step = "{\"id\": \"s1\", \"intent\": \"summarise\", \"tool\": null, \
                    \"args_template\": {}, \"depends_on\": [], \
                    \"expected\": {\"predicates\": [], \"assertion\": \"\", \
                    \"on_surprise\": \"continue\", \"confidence\": 1.0}}";
        let dag = parse_plan_dag(&plan_json(&format!("[{step}]")), &aliases()).unwrap();
        assert!(dag.steps[0].tool.is_none());
    }

    #[test]
    fn zero_step_plan_is_legal() {
        let dag = parse_plan_dag(&plan_json("[]"), &aliases()).unwrap();
        assert!(dag.steps.is_empty());
    }

    #[test]
    fn dependency_validation_catches_bad_edges() {
        // Unknown dep.
        let text = plan_json(&format!(
            "[{}]",
            one_step("s1", "workspaces.rag_query", "[\"s9\"]")
        ));
        assert!(parse_plan_dag(&text, &aliases())
            .unwrap_err()
            .contains("unknown step"));
        // Self dep.
        let text = plan_json(&format!(
            "[{}]",
            one_step("s1", "workspaces.rag_query", "[\"s1\"]")
        ));
        assert!(parse_plan_dag(&text, &aliases())
            .unwrap_err()
            .contains("depends on itself"));
        // Cycle.
        let text = plan_json(&format!(
            "[{}, {}]",
            one_step("s1", "workspaces.rag_query", "[\"s2\"]"),
            one_step("s2", "workspaces.rag_query", "[\"s1\"]")
        ));
        assert!(parse_plan_dag(&text, &aliases())
            .unwrap_err()
            .contains("cycle"));
    }

    #[test]
    fn duplicate_ids_and_oversize_plans_fail() {
        let text = plan_json(&format!(
            "[{}, {}]",
            one_step("s1", "workspaces.rag_query", "[]"),
            one_step("s1", "workspaces.rag_query", "[]")
        ));
        assert!(parse_plan_dag(&text, &aliases())
            .unwrap_err()
            .contains("duplicate"));

        let steps: Vec<String> = (0..MAX_PLAN_STEPS + 1)
            .map(|i| one_step(&format!("s{i}"), "workspaces.rag_query", "[]"))
            .collect();
        let text = plan_json(&format!("[{}]", steps.join(",")));
        assert!(parse_plan_dag(&text, &aliases())
            .unwrap_err()
            .contains("max"));
    }

    #[test]
    fn garbage_returns_readable_reason() {
        assert!(parse_plan_dag("", &aliases()).is_err());
        assert!(
            parse_plan_dag("I could not make a plan, sorry.", &aliases())
                .unwrap_err()
                .contains("no JSON object")
        );
        assert!(parse_plan_dag("{\"goal\": \"g\"}", &aliases())
            .unwrap_err()
            .contains("did not match the schema"));
    }

    #[test]
    fn step_detail_renders_tool_expectation_and_deps() {
        let step = PlanStep {
            id: "s2".into(),
            intent: "find the loader".into(),
            tool: Some("workspaces.symbols.find".into()),
            args_template: serde_json::json!({}),
            depends_on: vec!["s1".into()],
            expected: ExpectedOutcome {
                predicates: vec![OutcomePredicate::NonEmpty],
                assertion: String::new(),
                on_surprise: SurpriseAction::Replan,
                confidence: 0.9,
            },
        };
        assert_eq!(
            step_detail(&step, 1),
            "step 2 · workspaces.symbols.find · expects 1 check(s) · after s1"
        );
    }

    #[test]
    fn balanced_scan_ignores_braces_inside_strings() {
        let text = "note {\"goal\": \"a } tricky { goal\", \"steps\": [], \
                    \"reasoning_trace\": \"\", \"plan_version\": 1} tail";
        let dag = parse_plan_dag(text, &aliases()).unwrap();
        assert_eq!(dag.goal, "a } tricky { goal");
    }
}
