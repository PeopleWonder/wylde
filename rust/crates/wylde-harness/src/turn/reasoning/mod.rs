//! Agentic reasoning layer — gate + phase orchestration (implementation
//! plan §1, harness submodule NOT a crate, per
//! `dispatch_no_new_service_crates_for_harness`).
//!
//! Slice S1 ships the *configuration* surface only:
//!
//! * [`config`] — [`ReasoningConfig`] / [`ModelSlots`] / [`ReasonMode`] /
//!   [`Depth`], persisted at `<data_dir>/settings/reasoning.json`
//!   (RoutingConfig pattern), written through the
//!   `settings.reasoning.{get,set}` facade verbs.
//! * [`fit`] — the pure VRAM fit picker behind the `reasoning.fit_check`
//!   verb ([`handle_fit_check`]).
//! * [`resolve_depth`] — the payload → config → Fast resolution chain,
//!   consumed by `chat.start_turn` / `chat.run_turn` (parsed and logged in
//!   S1; the S3 plan phase is the first consumer that *acts* on it).
//! * [`constrained`] — grammar-constrained decoding plumbing (2026-07-13):
//!   [`constrained::plan_format`] (the `constrained_plan`-gated PlanDag
//!   schema) + [`constrained::ollama_chat_maybe_constrained`] (the
//!   fail-soft `format`-carrying chat call the PLAN phase makes). The
//!   post-turn memory extractor and the conversation auto-summariser also
//!   call the wrapper live (2026-07-13, policy table in [`constrained`]'s
//!   module docs).
//!
//! Slice S3 ships the PLAN phase itself:
//!
//! * [`inputs`] — the grounded [`inputs::PlanInputs`] bundle (live
//!   concepts + explicit IS-NOT exclusions + containment ladders +
//!   lessons + catalog + context digest) and the prompt rendering.
//! * [`plan_phase`] — the one reasoner call → validated [`PlanDag`]
//!   (constrained decoding, ≤32k ctx, fail-soft to plain ReAct).
//! * [`template`] — `${stepid.output.path}` placeholder resolution.
//! * [`ReasoningState`] + [`maybe_plan`] — the driver-facing seam: the
//!   gate, the plan, and the per-round step guidance / result recording
//!   the streaming loop consumes (open-loop in S3 — steps guide, outcomes
//!   are unchecked until S4's surprise detector).
//!
//! Slice S2 ships residency:
//!
//! * [`residency`] — warm model slots (plan §6.3a): one `ollama.preload`
//!   with `keep_alive:"24h"` per distinct slot model on boot / slot
//!   commit, so a Deep turn's reasoner is resident before it's needed.
//!   Also unifies the embedder definition
//!   ([`crate::memory::common::embed_model`]: env override → slot) and
//!   refines the fit probe with measured `/api/ps` footprints.
//!
//! Slice S4 closes the loop — surprise detection + replan-on-surprise:
//!
//! * [`surprise`] — the layered detector (L0 tool-failure shape, L1 pure
//!   `evaluate()` over declared predicates, conservatively-gated L2
//!   fast-model yes/no, L3 budget + no-progress) and the budget-gated
//!   replan response ([`plan_phase::replan`] → a revised
//!   [`PlanDag`], `plan_version` bumped, spliced into the running
//!   [`ReasoningState`]). Exhaustion degrades to plain ReAct with a
//!   visible notice; a planner-declared `abort` action ends the turn
//!   cleanly (`AbortReason::PlanPrecondition`). Everything else is
//!   fail-soft: detector/L2/replan failures all continue the turn.
//! * `auto_escalate` (config, OQ-5) stays **inert** after S4 — a Fast
//!   turn that self-escalates would break the Fast-tier byte-identity
//!   guarantee the e2e transcript test pins. Deliberately deferred until
//!   Aaron rules on how those two coexist; see the S4 slice report.
//!
//! **Identity guarantee:** with `ReasoningConfig.enabled == false` (the
//! default) or `depth == Fast`, nothing in this module touches the turn —
//! [`deep_gate_open`] is the single gate expression, [`maybe_plan`]
//! returns `None` without doing anything, and every driver-side touch is
//! behind `if let Some(state)`. The fast path stays byte-identical to
//! trunk; plain vector RAG + plain ReAct.

pub mod config;
pub mod constrained;
pub mod fit;
pub mod inputs;
pub mod plan_phase;
pub mod residency;
pub mod surprise;
pub mod template;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use wylde_reasoning_plan::{PlanDag, PlanStep};
use wylde_shared::ipc::{self, Reply};

use crate::state::TurnHandle;

pub use config::{Depth, ModelSlots, ReasonMode, ReasoningConfig, ReflectGate};
pub use fit::{fit, SlotFit};
pub use plan_phase::PlanOutcome;

/// Resolve the turn's reasoning depth: payload `"depth"` → config
/// `default_depth` → `Fast`. Mirrors `resolve_model`'s payload-then-config
/// fallback. Malformed payload values fall through (tolerant, never fail a
/// turn on a bad flag).
pub fn resolve_depth(payload: &Value) -> Depth {
    payload
        .get("depth")
        .and_then(Value::as_str)
        .and_then(Depth::parse)
        .unwrap_or_else(|| ReasoningConfig::current().default_depth)
}

/// The one gate expression (plan §2): a turn enters PLAN only when the
/// resolved depth is a planning TIER (anything above `Fast`) **and** the
/// master toggle is on. `false` ⇒ today's exact path — plain vector RAG,
/// plain ReAct, byte-identical.
pub fn deep_gate_open(depth: Depth) -> bool {
    depth.plans() && ReasoningConfig::current().enabled
}

/// Multiplier applied to on-disk model size to estimate resident bytes —
/// the same convention as `wylde-ollama`'s estimate path
/// (`WYLDE_OLLAMA_VRAM_ESTIMATE_MULT`, default 1.2).
fn vram_estimate_mult() -> f64 {
    std::env::var("WYLDE_OLLAMA_VRAM_ESTIMATE_MULT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|m| *m > 0.0)
        .unwrap_or(1.2)
}

/// The broker service the budget probe asks. Same default as
/// `wylde-ollama`'s `broker_service`.
fn broker_service() -> String {
    std::env::var("WYLDE_HARNESS_BROKER_SERVICE").unwrap_or_else(|_| "wylde-vram-broker".to_owned())
}

/// `reasoning.fit_check {slots?, mode?}` — price the (given or configured)
/// slot set against the live VRAM budget. Reply: the serialized
/// [`SlotFit`]. **Fail-soft end to end**: an unreachable Ollama prices
/// every model unknown, an unreachable broker reports budget 0 — both
/// degrade to warnings in the verdict, never an error reply. Advisory
/// only; nothing gates on it (readiness-chip pattern).
pub async fn handle_fit_check(payload: Value) -> Reply {
    let cfg = ReasoningConfig::current();
    // Optional overrides: price a combo the user is *considering* without
    // persisting it first.
    let slots = payload
        .get("slots")
        .map(|v| {
            serde_json::from_value::<ModelSlots>(v.clone()).unwrap_or_else(|_| cfg.slots.clone())
        })
        .unwrap_or_else(|| cfg.slots.clone());
    let mode = payload
        .get("mode")
        .and_then(Value::as_str)
        .and_then(|s| match s {
            "split" => Some(ReasonMode::Split),
            "single" => Some(ReasonMode::Single),
            _ => None,
        })
        .unwrap_or(cfg.mode);

    let sizes = probe_model_sizes().await;
    let budget = probe_vram_budget().await;
    let verdict = fit::fit(&slots, mode, budget, &sizes);
    Reply::ok(serde_json::to_value(&verdict).unwrap_or_else(|_| json!({})))
}

/// Estimated resident bytes per pulled model tag: `ollama.list_models`
/// (`/api/tags` passthrough) `models[].{name,size}` × the estimate mult,
/// then REFINED with `ollama.list_loaded` (`/api/ps`): a currently-loaded
/// model's measured resident footprint replaces the disk-based guess (S2
/// estimator refinement — the ×1.2 disk multiplier over-prices dynamic
/// MoE quants and under-prices long-context loads; the live number is the
/// truth when we have it). Unreachable / malformed ⇒ empty map (every
/// model "unknown", warned) / estimates kept.
async fn probe_model_sizes() -> HashMap<String, u64> {
    let harness_cfg = crate::config::Config::get();
    let mult = vram_estimate_mult();
    let reply =
        ipc::send_action(&harness_cfg.ollama_service, "ollama.list_models", json!({})).await;
    if !reply.ok {
        return HashMap::new();
    }
    let mut sizes: HashMap<String, u64> = reply
        .data
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name").and_then(Value::as_str)?;
                    let size = m.get("size").and_then(Value::as_u64).filter(|&n| n > 0)?;
                    Some((name.to_owned(), (size as f64 * mult) as u64))
                })
                .collect()
        })
        .unwrap_or_default();
    let loaded =
        ipc::send_action(&harness_cfg.ollama_service, "ollama.list_loaded", json!({})).await;
    if loaded.ok {
        merge_measured_sizes(&mut sizes, &loaded.data);
    }
    sizes
}

/// Overlay measured `/api/ps` footprints onto the disk-based estimates.
/// `size` is the model's total resident memory (GPU + any DRAM spill) —
/// the honest number to price against a VRAM budget for a model that is
/// loaded right now.
fn merge_measured_sizes(sizes: &mut HashMap<String, u64>, ps_reply: &Value) {
    let Some(models) = ps_reply.get("models").and_then(Value::as_array) else {
        return;
    };
    for m in models {
        let Some(name) = m.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(size) = m.get("size").and_then(Value::as_u64).filter(|&n| n > 0) else {
            continue;
        };
        sizes.insert(name.to_owned(), size);
    }
}

/// GPU budget from the broker's `vram.state` (`gpu.total_bytes`).
/// Unreachable ⇒ 0 (fit reports "budget unknown").
async fn probe_vram_budget() -> u64 {
    let reply = ipc::send_action(&broker_service(), "vram.state", json!({})).await;
    if !reply.ok {
        return 0;
    }
    reply
        .data
        .get("gpu")
        .and_then(|g| g.get("total_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

// ── S3: the driver-facing plan-execution seam ───────────────────────────

/// One executed step's record for the replan prompt: id, tool, and a
/// truncated result digest. Kept in execution order across plan
/// revisions (a v2 replan prompt still shows what v1 executed).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedStep {
    pub id: String,
    pub tool: Option<String>,
    pub digest: String,
}

/// What one round did to the plan — handed by [`ReasoningState::finish_round`]
/// to the S4 outcome check.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoundCompletion {
    /// The step whose guidance rode the round (`None` = no plan step was
    /// in flight — nothing to check).
    pub step_id: Option<String>,
    /// A result was recorded for that step. `false` with a `step_id` means
    /// the round dispatched calls but every one was duplicate-suppressed —
    /// the L3 no-progress signal.
    pub completed: bool,
}

/// Per-turn reasoning state, threaded through the streaming loop NEXT TO
/// `ToolRoundState`, never merged into it (the fast path's state stays
/// untouched). Owns the plan, the execution cursor, and (S4) the surprise
/// bookkeeping: replan budget used, per-step L2 marks, the executed-step
/// log for replan prompts, and the abandoned flag (budget exhaustion ⇒
/// plain ReAct).
pub struct ReasoningState {
    pub dag: PlanDag,
    /// Step results by id — feeds [`template::resolve`] for later steps'
    /// `${sid.output…}` placeholders. Retained across plan revisions.
    results: HashMap<String, Value>,
    /// Ids of steps considered executed. Retained across plan revisions
    /// (a revised plan reusing a completed id is skipped, not re-run).
    completed: HashSet<String>,
    /// The step whose guidance rode the current round, if any.
    in_flight: Option<String>,
    /// The PLAN call's cost, for the driver's turn meter (honest latency
    /// accounting — the deep turn pays for its planning call visibly).
    pub plan_prompt_tokens: u64,
    pub plan_completion_tokens: u64,
    pub plan_elapsed_ms: u64,
    /// The grounded inputs PLAN was prompted with — replan prompts reuse
    /// the goal / exclusions / catalog without re-gathering (S4).
    plan_inputs: inputs::PlanInputs,
    /// Execution-ordered log of (id, tool, result digest) — the replan
    /// prompt's "Executed step results" block (S4).
    executed_log: Vec<ExecutedStep>,
    /// Replans consumed this turn, against `ReasoningConfig.replan_budget`.
    replans_used: u8,
    /// Budget exhausted: guidance stops, the loop runs plain ReAct.
    abandoned: bool,
    /// Steps whose L2 check already ran (one L2 per step max).
    l2_checked: HashSet<String>,
}

impl ReasoningState {
    fn new(outcome: PlanOutcome) -> Self {
        Self {
            dag: outcome.dag,
            results: HashMap::new(),
            completed: HashSet::new(),
            in_flight: None,
            plan_prompt_tokens: outcome.prompt_tokens,
            plan_completion_tokens: outcome.completion_tokens,
            plan_elapsed_ms: outcome.elapsed_ms,
            plan_inputs: outcome.plan_inputs,
            executed_log: Vec::new(),
            replans_used: 0,
            abandoned: false,
            l2_checked: HashSet::new(),
        }
    }

    /// Splice a replanned DAG in (S4): the revision replaces the step
    /// graph; results, the completed set and the executed log are
    /// retained so `${sid.output…}` placeholders keep resolving and a
    /// revision that reuses a completed id skips it instead of re-running.
    pub(crate) fn adopt_revised_plan(&mut self, dag: PlanDag) {
        self.dag = dag;
        self.in_flight = None;
    }

    /// The next not-yet-executed step whose dependencies are all
    /// satisfied. Plan order breaks ties (the DAG is validated acyclic, so
    /// progress is guaranteed).
    fn next_ready_step(&self) -> Option<&PlanStep> {
        self.dag.steps.iter().find(|s| {
            !self.completed.contains(&s.id)
                && s.depends_on.iter().all(|d| self.completed.contains(d))
        })
    }

    /// Round-entry seam: auto-advance past any non-final synthesis beats
    /// (a mid-plan `tool: null` step can't round-trip a ReAct round —
    /// its narrative rides the next guidance), then render the current
    /// step's guidance as a tail message. `None` when the plan is spent
    /// or abandoned (S4 budget exhaustion) — the loop continues as plain
    /// ReAct.
    ///
    /// The message is `role: user` on the TAIL of the round's messages —
    /// never the system message — so Ollama's KV prefix over
    /// `system + history` survives every Deep round (plan §9 R5).
    pub fn begin_round(&mut self) -> Option<Value> {
        if self.abandoned {
            return None;
        }
        loop {
            let (id, is_synthesis) = {
                let step = self.next_ready_step()?;
                (step.id.clone(), step.tool.is_none())
            };
            let remaining_after = self
                .dag
                .steps
                .iter()
                .any(|s| s.id != id && !self.completed.contains(&s.id));
            if is_synthesis && remaining_after {
                // Non-final synthesis beat: complete it silently and move on.
                self.completed.insert(id);
                continue;
            }
            self.in_flight = Some(id.clone());
            let step = self
                .dag
                .steps
                .iter()
                .find(|s| s.id == id)
                .expect("in-flight step exists");
            return Some(json!({
                "role": "user",
                "content": guidance_text(step, &self.results, &self.dag),
            }));
        }
    }

    /// Post-dispatch seam: fold the round's tool results into the
    /// in-flight step. The step's recorded output is the first result
    /// whose canonical tool name matches the step's `tool` (else the
    /// round's first result), parsed as JSON when possible so
    /// `${sid.output.path}` can drill in.
    ///
    /// The returned [`RoundCompletion`] feeds the S4 outcome check: which
    /// step just realised a result (evaluate its expectation), or that
    /// the round's calls were all duplicate-suppressed (`completed:
    /// false` — the L3 no-progress signal; the step stays open).
    pub(crate) fn finish_round(&mut self, round_results: &[(String, String)]) -> RoundCompletion {
        let Some(id) = self.in_flight.take() else {
            return RoundCompletion {
                step_id: None,
                completed: false,
            };
        };
        if round_results.is_empty() {
            // Every dispatched call was duplicate-suppressed (a round
            // with NO calls never reaches this seam — the driver
            // finalizes instead). Leave the step open and let the
            // outcome check decide (replan-or-degrade).
            return RoundCompletion {
                step_id: Some(id),
                completed: false,
            };
        }
        let step_tool = self
            .dag
            .steps
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.tool.clone());
        let chosen = step_tool
            .and_then(|t| round_results.iter().find(|(name, _)| *name == t))
            .or_else(|| round_results.first())
            .map(|(_, content)| content.clone())
            .unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&chosen).unwrap_or(Value::String(chosen));
        self.executed_log.push(ExecutedStep {
            id: id.clone(),
            tool: self
                .dag
                .steps
                .iter()
                .find(|s| s.id == id)
                .and_then(|s| s.tool.clone()),
            digest: surprise::digest_value(&parsed, surprise::REPLAN_DIGEST_MAX_CHARS),
        });
        self.results.insert(id.clone(), parsed);
        self.completed.insert(id.clone());
        RoundCompletion {
            step_id: Some(id),
            completed: true,
        }
    }
}

/// Render one step's guidance block. The fast model still emits the real
/// tool call (Plan-and-Execute, OQ-3) — the salvage net, dedupe, tier and
/// consent gates in `tool_round` remain the only dispatch authority, and
/// the model may deviate when the suggestion is wrong.
fn guidance_text(step: &PlanStep, results: &HashMap<String, Value>, dag: &PlanDag) -> String {
    let position = dag
        .steps
        .iter()
        .position(|s| s.id == step.id)
        .map(|i| i + 1)
        .unwrap_or(0);
    let total = dag.steps.len();
    let mut s = format!(
        "[plan step {position}/{total} — {id}] {intent}",
        id = step.id,
        intent = step.intent
    );
    match &step.tool {
        Some(tool) => {
            let args = template::resolve(&step.args_template, results);
            s.push_str(&format!(
                "\nSuggested tool call: {tool} with arguments {args}",
                args = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_owned())
            ));
        }
        None => s.push_str(
            "\nSynthesis step — no tool call: compose the answer from the results so far.",
        ),
    }
    let assertion = step.expected.assertion.trim();
    if !assertion.is_empty() {
        s.push_str(&format!("\nExpected outcome: {assertion}"));
    }
    s.push_str(
        "\nExecute this step now. If the suggestion is wrong for the goal, do the right thing instead.",
    );
    s
}

/// The single driver-facing entry point (seam 1, plan §1): evaluate the
/// gate, and on a Deep+enabled turn run the PLAN phase. Every skip path —
/// toggle off, Fast depth, planner failure, zero-step plan — returns
/// `None`, and the caller's loop runs verbatim.
#[allow(clippy::too_many_arguments)] // mirrors the turn-driver fan-out it's called from
pub(crate) async fn maybe_plan(
    cfg: &'static crate::config::Config,
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    depth: Depth,
    workspace_id: Option<&str>,
    user_message: &str,
    gathered: &crate::turn::context_gather::GatheredContext,
    alias_map: &HashMap<String, String>,
) -> Option<ReasoningState> {
    if !deep_gate_open(depth) {
        return None;
    }
    let outcome = plan_phase::run(
        cfg,
        handle,
        turn_id,
        depth,
        workspace_id,
        user_message,
        gathered,
        alias_map,
    )
    .await?;
    if outcome.dag.steps.is_empty() {
        // Legal "answer directly" plan — fall through to plain ReAct
        // (the trace was already surfaced as a Thinking event).
        return None;
    }
    tracing::info!(
        turn_id = %turn_id,
        steps = outcome.dag.steps.len(),
        elapsed_ms = outcome.elapsed_ms,
        prompt_tokens = outcome.prompt_tokens,
        completion_tokens = outcome.completion_tokens,
        "reasoning: PLAN phase produced a plan"
    );
    Some(ReasoningState::new(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_depth_payload_wins() {
        assert_eq!(resolve_depth(&json!({ "depth": "think" })), Depth::Think);
        assert_eq!(
            resolve_depth(&json!({ "depth": "ultrathink" })),
            Depth::Ultrathink
        );
        // Legacy wire value keeps its S3 semantics.
        assert_eq!(
            resolve_depth(&json!({ "depth": "deep" })),
            Depth::ThinkHarder
        );
        assert_eq!(resolve_depth(&json!({ "depth": "fast" })), Depth::Fast);
    }

    #[test]
    fn resolve_depth_falls_back_to_fast() {
        // No payload flag + default config (default_depth: Fast) ⇒ Fast.
        // (The config cache seeds from a missing file as default in the
        // test env — the identity default.)
        assert_eq!(resolve_depth(&json!({})), Depth::Fast);
        // Malformed values fall through the chain, never error.
        assert_eq!(resolve_depth(&json!({ "depth": "sideways" })), Depth::Fast);
        assert_eq!(resolve_depth(&json!({ "depth": 3 })), Depth::Fast);
    }

    #[test]
    fn gate_is_closed_by_default() {
        // Default config: enabled == false ⇒ even an explicit planning
        // tier does not open the gate. THE identity guard.
        for tier in [Depth::Think, Depth::ThinkHarder, Depth::Ultrathink] {
            assert!(!deep_gate_open(tier) || ReasoningConfig::current().enabled);
        }
        assert!(!deep_gate_open(Depth::Fast), "Fast never opens the gate");
    }

    #[test]
    fn estimate_mult_defaults() {
        // Not asserting against the env var (other tests may set it) —
        // just the parse contract on the default path.
        let m = vram_estimate_mult();
        assert!(m > 0.0);
    }

    #[test]
    fn measured_ps_footprint_overrides_disk_estimate() {
        // S2 estimator refinement: a loaded model's /api/ps `size` beats
        // the ×1.2 disk guess; unloaded models keep their estimates and
        // malformed entries are skipped.
        let mut sizes = HashMap::from([
            ("big-moe:35b".to_owned(), 16_936_774_726_u64), // disk × 1.2
            ("qwen2.5:7b-instruct".to_owned(), 5_000_000_000_u64),
        ]);
        merge_measured_sizes(
            &mut sizes,
            &json!({
                "models": [
                    { "name": "big-moe:35b", "size": 13_884_970_760_u64, "size_vram": 13_884_970_760_u64 },
                    { "name": "no-size" },
                    { "size": 1 }
                ]
            }),
        );
        assert_eq!(sizes["big-moe:35b"], 13_884_970_760, "measured wins");
        assert_eq!(sizes["qwen2.5:7b-instruct"], 5_000_000_000, "estimate kept");
        assert_eq!(sizes.len(), 2, "malformed entries ignored");

        // Fail-soft: a reply with no models array changes nothing.
        let before = sizes.clone();
        merge_measured_sizes(&mut sizes, &json!({}));
        assert_eq!(sizes, before);
    }

    // ── S3: ReasoningState (open-loop plan execution) ───────────────────

    use wylde_reasoning_plan::ExpectedOutcome;

    fn step(id: &str, tool: Option<&str>, deps: &[&str], args: Value) -> PlanStep {
        PlanStep {
            id: id.into(),
            intent: format!("do {id}"),
            tool: tool.map(str::to_owned),
            args_template: args,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            expected: ExpectedOutcome::trusting(),
        }
    }

    fn state(steps: Vec<PlanStep>) -> ReasoningState {
        ReasoningState::new(PlanOutcome {
            dag: PlanDag {
                goal: "g".into(),
                steps,
                reasoning_trace: String::new(),
                plan_version: 1,
            },
            prompt_tokens: 100,
            completion_tokens: 50,
            elapsed_ms: 1200,
            plan_inputs: inputs::PlanInputs::default(),
        })
    }

    #[test]
    fn guidance_walks_steps_in_dependency_order() {
        let mut rs = state(vec![
            step("s1", Some("fs.read"), &[], json!({"path": "a"})),
            step("s2", Some("fs.read"), &["s1"], json!({"path": "b"})),
        ]);
        let g1 = rs.begin_round().expect("first step guides");
        let text = g1["content"].as_str().unwrap();
        assert!(text.contains("[plan step 1/2 — s1]"), "{text}");
        assert!(text.contains("fs.read"), "{text}");
        assert_eq!(
            g1["role"], "user",
            "guidance rides the tail as a user message"
        );

        rs.finish_round(&[("fs.read".into(), "{\"ok\": true}".into())]);
        let g2 = rs.begin_round().expect("second step after dep satisfied");
        assert!(g2["content"].as_str().unwrap().contains("s2"));
        rs.finish_round(&[("fs.read".into(), "done".into())]);
        assert!(rs.begin_round().is_none(), "plan spent → plain ReAct");
    }

    #[test]
    fn placeholders_resolve_against_recorded_results() {
        let mut rs = state(vec![
            step("s1", Some("fs.list"), &[], json!({})),
            step(
                "s2",
                Some("fs.read"),
                &["s1"],
                json!({"path": "${s1.output.entries.0}"}),
            ),
        ]);
        rs.begin_round().unwrap();
        rs.finish_round(&[("fs.list".into(), "{\"entries\": [\"Cargo.toml\"]}".into())]);
        let g2 = rs.begin_round().unwrap();
        assert!(
            g2["content"]
                .as_str()
                .unwrap()
                .contains("{\"path\":\"Cargo.toml\"}"),
            "resolved args ride the guidance: {}",
            g2["content"]
        );
    }

    #[test]
    fn result_matching_prefers_the_steps_own_tool() {
        let mut rs = state(vec![
            step("s1", Some("fs.read"), &[], json!({})),
            step("s2", None, &["s1"], json!({})),
        ]);
        rs.begin_round().unwrap();
        // Round dispatched two calls; the step's own tool wins.
        rs.finish_round(&[
            ("memory.search".into(), "\"noise\"".into()),
            ("fs.read".into(), "\"the real one\"".into()),
        ]);
        assert_eq!(rs.results["s1"], json!("the real one"));
    }

    #[test]
    fn non_final_synthesis_steps_auto_advance() {
        let mut rs = state(vec![
            step("s1", None, &[], json!({})),
            step("s2", Some("fs.read"), &["s1"], json!({})),
        ]);
        // s1 is a mid-plan synthesis beat — skipped silently; guidance
        // lands on s2 directly.
        let g = rs.begin_round().unwrap();
        assert!(g["content"].as_str().unwrap().contains("s2"));
    }

    #[test]
    fn final_synthesis_step_guides_composition() {
        let mut rs = state(vec![
            step("s1", Some("fs.read"), &[], json!({})),
            step("s2", None, &["s1"], json!({})),
        ]);
        rs.begin_round().unwrap();
        rs.finish_round(&[("fs.read".into(), "x".into())]);
        let g = rs.begin_round().unwrap();
        let text = g["content"].as_str().unwrap();
        assert!(text.contains("Synthesis step — no tool call"), "{text}");
    }

    #[test]
    fn empty_round_leaves_step_open_and_meter_seeds() {
        let mut rs = state(vec![step("s1", Some("fs.read"), &[], json!({}))]);
        rs.begin_round().unwrap();
        // All the round's calls were duplicate-suppressed: the step stays
        // open and the completion flags the L3 no-progress signal.
        let c = rs.finish_round(&[]);
        assert_eq!(c.step_id.as_deref(), Some("s1"));
        assert!(!c.completed, "no result ⇒ not completed ⇒ no-progress");
        assert!(rs.results.is_empty());
        assert_eq!(rs.plan_prompt_tokens, 100);
        assert_eq!(rs.plan_completion_tokens, 50);
    }

    // ── S4: surprise bookkeeping on the state ───────────────────────────

    #[test]
    fn finish_round_reports_the_completed_step_and_logs_it() {
        let mut rs = state(vec![step("s1", Some("fs.read"), &[], json!({}))]);
        rs.begin_round().unwrap();
        let c = rs.finish_round(&[("fs.read".into(), "{\"entries\": [\"a\"]}".into())]);
        assert_eq!(c.step_id.as_deref(), Some("s1"));
        assert!(c.completed);
        assert_eq!(
            rs.executed_log,
            vec![ExecutedStep {
                id: "s1".into(),
                tool: Some("fs.read".into()),
                digest: "{\"entries\":[\"a\"]}".into(),
            }],
            "the executed log records id, tool and digest for replan prompts"
        );
        // No in-flight step ⇒ nothing to check.
        let c = rs.finish_round(&[("fs.read".into(), "x".into())]);
        assert_eq!(c.step_id, None);
    }

    #[test]
    fn adopt_revised_plan_keeps_results_and_walks_the_new_steps() {
        let mut rs = state(vec![
            step("s1", Some("fs.list"), &[], json!({})),
            step("s2", Some("fs.read"), &["s1"], json!({})),
        ]);
        rs.begin_round().unwrap();
        rs.finish_round(&[("fs.list".into(), "{\"entries\": [\"Cargo.toml\"]}".into())]);

        // The revision replaces the remainder; a fresh id chains onto the
        // OLD step's recorded result via placeholders.
        rs.adopt_revised_plan(PlanDag {
            goal: "g".into(),
            steps: vec![step(
                "r1",
                Some("fs.read"),
                &[],
                json!({"path": "${s1.output.entries.0}"}),
            )],
            reasoning_trace: String::new(),
            plan_version: 2,
        });
        assert_eq!(rs.dag.plan_version, 2);
        let g = rs.begin_round().expect("revised step guides");
        let text = g["content"].as_str().unwrap();
        assert!(text.contains("[plan step 1/1 — r1]"), "{text}");
        assert!(
            text.contains("{\"path\":\"Cargo.toml\"}"),
            "old results still resolve placeholders: {text}"
        );
    }

    #[test]
    fn adopt_revised_plan_skips_reused_completed_ids() {
        let mut rs = state(vec![step("s1", Some("fs.read"), &[], json!({}))]);
        rs.begin_round().unwrap();
        rs.finish_round(&[("fs.read".into(), "x".into())]);
        // A revision that (against instructions) reuses the completed id
        // skips it rather than re-running.
        rs.adopt_revised_plan(PlanDag {
            goal: "g".into(),
            steps: vec![
                step("s1", Some("fs.read"), &[], json!({})),
                step("r1", Some("fs.list"), &[], json!({})),
            ],
            reasoning_trace: String::new(),
            plan_version: 2,
        });
        let g = rs.begin_round().unwrap();
        assert!(g["content"].as_str().unwrap().contains("r1"));
    }

    #[test]
    fn abandoned_state_stops_guidance() {
        let mut rs = state(vec![step("s1", Some("fs.read"), &[], json!({}))]);
        rs.abandoned = true;
        assert!(
            rs.begin_round().is_none(),
            "budget exhaustion ⇒ plain ReAct, no more guidance"
        );
    }
}
