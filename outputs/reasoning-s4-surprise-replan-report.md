# Agentic Reasoning S4 — Surprise Detection + Replan-on-Surprise: Slice Report

**Date:** 2026-07-14 · **Branch:** `feat/tbs-reasoning-surprise` · **Base:** trunk @ `9583932` (post-tiers)
**Plan:** `docs/plans/agentic-reasoning-implementation-plan.md` §4 + §8 S4 (scope P5)

---

## What shipped — the loop is closed

S3 executed plans **open-loop**: steps guided the ReAct rounds, but nothing checked whether
outcomes matched. S4 wires the check: after every guided round, the realised tool result is
evaluated against the step's plan-time `ExpectedOutcome`, and a confirmed surprise hands
{original plan, executed results, verdict} back to the reasoner for a **revised PlanDag** that
splices into the running turn. Cheap detect / expensive respond, exactly as scoped.

| piece | where |
|---|---|
| detector + response orchestration | `turn/reasoning/surprise.rs` (new) |
| REPLAN call + prompt | `plan_phase.rs` — `replan()` + `render_replan_prompt()`; PLAN and REPLAN share one `tiered_dag_call` core (tier knobs, grammar constraint, think-exhaustion salvage, parse ladder — a factoring, behaviour pinned by the S3 e2e suite) |
| state bookkeeping | `ReasoningState` — executed-step log, replan budget, per-step L2 marks, `abandoned` flag; `adopt_revised_plan` retains results/completed so `${sid.output…}` resolves across plan versions |
| driver seam 3b | `actions.rs` — outcome check after `finish_round`, entirely behind `if let Some(state)`; L2/replan tokens fold into the turn meter |
| clean abort | `AbortReason::PlanPrecondition` (new enum arm; GUI renders reason strings raw, so older builds degrade gracefully) |

## How surprise is actually MEASURED (the concrete answer)

Per executed plan step, in order, **zero model calls unless stated**:

1. **L0 — deterministic tool failure.** The recorded step result is a failure when it is a
   string prefixed `[error]` / `[tier_blocked]` (exactly what `run_one_tool` renders for a
   failed or gate-blocked dispatch) **or** a structural error envelope
   (`wylde_reasoning_plan::is_error_envelope`: non-empty `"error"` field, `"ok": false`, or
   `"status": "error"`). A failed tool is surprising regardless of what the planner declared.
2. **L1 — the declared predicates.** The pure `evaluate(&step.expected, &result)` from P0 —
   `non_empty`, `json_path_exists`, `json_path_equals`, `contains`, `count_at_least`,
   `no_error`, ALL must hold. Every failed predicate is collected (no short-circuit) and
   rendered human-readable into the Step detail and the replan prompt
   (`/entries has >= 1 item(s)`). **This is the primary signal**: the planner wrote down, at
   plan time, what a non-surprising result looks like; the check is a pure function of that
   declaration and the actual result.
3. **L2 — one gated fast-model yes/no** (the only model-cost layer; policy below).
4. **L3 — budget + no-progress.** `replans_used >= replan_budget` trips the visible degrade;
   independently, a round whose dispatched calls were **all duplicate-suppressed** (the model
   re-issuing an already-run `(name,args)` — zero new results with the step still open) is a
   no-progress surprise routed straight to replan-or-degrade. Without this, a stuck model
   ping-pongs the same call to `MAX_TOOL_LOOPS`.

Embedding-distance checks are absent by design (plan §4: cost + uncalibrated threshold; the
predicates encode the expectation more precisely than a cosine can).

## The L2 gating policy

L2 fires **only** when all of:

* L0 and L1 found nothing (`!surprised`), **and**
* the step declared a non-empty `assertion`, **and**
* L1 was inconclusive — the step declared **no predicates** (assertion-only), or its
  predicates passed but planner `confidence ≤ 0.75` (`L2_CONFIDENCE_THRESHOLD`, the
  plausible-but-wrong case), **and**
* this step has not been L2-checked before (once per step, ever).

The call itself honours every lesson this track has bought: **`think:false`** (a two-field
verdict has nothing to deliberate on; a tight think CAP is an empty-output machine),
`num_predict: 256`, result digest truncated to 2,000 chars (~500 tokens), and a
**grammar-constrained closed schema** `{"satisfied": bool, "reason": string}`
(`l2_verdict_schema`, gated on the same `constrained_plan` toggle as PLAN — the constrained.rs
policy table row is now live). A backend that rejects the `think` switch gets the standard
one-retry-without-it. Any L2 failure — IPC error, unparseable verdict — counts as
**satisfied** (fail-soft: a broken checker must never manufacture surprises). Live
spot-check: the default reasoner answers the planted-empty case `satisfied:false` and the
planted-good case `satisfied:true`, both with sensible reasons.

## The response path

Surprise confirmed ⇒ visible `Step(Reasoning)` ("s1 surprised: 1 expected check(s) failed",
detail = failed checks + result digest), then the step's plan-time `on_surprise`:

* **`continue`** — logged, execution proceeds (soft expectation).
* **`abort`** — clean `TurnAborted` with the new `plan_precondition` reason (the planner
  marked the step an unrecoverable precondition). The only turn-ending path in the module.
* **`replan`** — budget-gated (`replan_budget`, default 2): emit `Phase(Replanning)` +
  "Replanning (1 of 2)…", then ONE reasoner call at the turn's tier via the same constrained
  + salvage machinery as PLAN. The prompt reuses the retained `PlanInputs` (goal, IS-NOT
  exclusions, tool catalog — no re-gather, no second IPC fan-out) plus: the plan under
  revision with done/pending marks, every executed step's result digest (≤400 chars each),
  the surprise verdict, and instructions (fresh ids, reference old results via placeholders,
  route around the surprising call, `steps: []` if the goal is now answerable). The revision
  bumps `plan_version` and splices in with results retained. The revised checklist re-renders
  in the Plan dropdown ("Plan revised (v2): 1 step(s) in 4.4s").

**Loop guard:** replans cap at `replan_budget` per turn (the cap spans plan versions — a v2
that surprises spends from the same pot). On exhaustion: visible
"Replan budget exhausted (2) — continuing without the plan", the state flips `abandoned`,
guidance stops, and the loop runs plain ReAct to natural completion. A failed replan call
keeps executing the *current* plan. `MAX_TOOL_LOOPS` (8) remains the outer hard stop.

## Cost honesty — live numbers (dev rig RTX 5080, default reasoner UD-IQ3_XXS, warm, constrained; ~925-token grounded prompt, 3 samples think / 2 samples think_harder)

| call | think tier (the default) | think_harder |
|---|---|---|
| PLAN | 5.3 / 6.8 / 9.8 s (mean **7.3 s**) | 18.3 / 21.1 s |
| REPLAN | 2.3 / 4.9 / 5.8 s (mean **4.4 s**) | 21.9 / 28.5 s |
| L2 verdict | 0.6–0.8 s (~131 prompt + ~40 eval tok) | — (L2 always runs think-off) |

All 10/10 plan-shaped calls grammar-valid. So at the recommended default (`think`):
**a clean deep turn pays ~5–10 s of planning; a surprising turn pays roughly +4–6 s more**
(replan ~4.4 s, + ~0.7 s if the surprise came via L2) — a surprising turn's reasoning
overhead lands around **10–15 s total**, i.e. the surprise roughly doubles the planning cost
but stays far under the old Deep tier's single plan call. At `think_harder` a surprise is
expensive: the replan ruminates on the bigger prompt (13–15k think chars measured), so
+22–28 s per replan on top of the ~20 s plan — a surprising think_harder turn is ~40–50 s of
reasoner time. Worst case is still bounded: budget 2 × tier cap + salvage. All of it lands on
the visible token meter (the e2e asserts the exact fold).

## Identity proof (the non-negotiable)

* The e2e transcript test (extended, green): toggle OFF / depth Fast ⇒ event transcripts and
  every Ollama request body byte-identical to baseline, zero unary calls, zero `format` keys —
  now additionally asserting zero replanning phases, zero surprise/replan steps, zero
  `plan_precondition` aborts in every gate-closed transcript.
* Structural: the entire S4 seam is inside `if let Some(rs) = &mut reasoning_state`; a Fast
  turn never constructs the state, so the check code is unreachable. No unconditional
  additions to the fast path at all in this slice.

## Tests

* harness lib **1150 / 0** (tiers baseline 1140 + 10: L0 shape matrix, digest char-boundary
  truncation, predicate descriptions, L2 schema↔serde lockstep, replan-prompt golden +
  empty-section omission, executed-log recording, revised-plan adoption incl. cross-version
  placeholder chains + reused-id skip, abandoned-state guidance stop, no-progress completion);
* `reasoning_plan_e2e` **12 / 12** (+6: planted-failure→replan with full prompt-content
  assertions, L2 gated + satisfied, L2 dissatisfied→replan, budget-2 exhaustion→visible
  degrade with no fourth guidance, abort→clean `plan_precondition`, duplicate-round
  no-progress→replan; the pre-existing clean-run test doubles as the planted-clean proof —
  its `unary_count == 1` assertion fails if anything false-trips);
* `run_turn_loop_e2e` 6/6, `tool_dispatch_e2e` 2/2, plan crate 21/21; fmt clean; clippy adds
  no new warnings (the reasoning-module hits are pre-existing config/residency lines).

## Deviations / notes for Aaron

* **`auto_escalate` stays inert — needs your one-line ruling.** The plan put fast→deep
  auto-escalation at the tail of S4, but it directly contradicts the standing non-negotiable
  this brief restated: *Fast tier ⇒ byte-identical behaviour*. An enabled+Fast turn that
  self-escalates on a double hard-failure is, by definition, not byte-identical. Rather than
  quietly weaken the identity contract, I left the config knob documented-inert
  (`config.rs` explains why). Options: (a) keep it inert and drop OQ-5; (b) authorize a
  narrowed contract — "enabled+Fast is identical except after ≥2 hard tool failures on the
  same intent" — and I wire it in a follow-up slice.
* **No-progress surprises ignore the step's `on_surprise`** and go straight to
  replan-or-degrade: the declared action describes the step's *result* semantics, and a
  `continue` there would loop the duplicate forever.
* **⚠️ Worktree directories mass-deleted mid-session (not by me).** While this slice was in
  flight, the directories of nearly every registered worktree under `C:\Users\aaron\wylde-wt-*`
  vanished (only `wylde-wt-temporal` and the vault-local `wylde-wt-lexical` survive on disk);
  `git worktree list` now shows ~15 entries `prunable`. **No commits or branches were lost** —
  only working directories (and, for me, two uncommitted doc comments, re-applied). I
  re-created my worktree and finished, and did NOT prune the other registrations — your call
  whether that was an intentional cleanup. If a cleanup tool did this, worktrees under the
  user-profile root may not be a safe location.
* Nextcloud status board NOT updated: `nctool` reports the `Nextcloud-AppPassword`
  credential is not configured in this session's Credential Manager — the same gap as the
  S1–S3.5 sessions. The plan-doc S4 row (updated in the main-dir working copy;
  `docs/plans/` is untracked by design) carries the status until the credential returns.
* Measurement scripts + raw `results.json` in the session scratchpad (`measure/`).

## What's next

S5 — REFLECT: critique call on the reasoner slot (`"reasoning.critique"` prompt key),
`ReflectGate` (default MultiToolOnly), one gap round max, lesson → long-term store through
the τ=0.92 dedup path so it resurfaces as a future turn's `PlanInputs.lessons`.
