# Agentic Reasoning S4b — Fast→Planning Auto-Escalation: Slice Report

**Date:** 2026-07-14 · **Branch:** `feat/tbs-reasoning-auto-escalate` (commit `6de32b6`) · **Base:** trunk @ `dee87c4` (post-S4)
**Authority:** Aaron's 2026-07-14 ruling — option (b), the NARROWED identity contract:
*"reasoning enabled + Fast tier is byte-identical to today EXCEPT after ≥2 hard tool failures, at which point the turn escalates to planning."*

---

## What shipped

`auto_escalate` (scope OQ-5) is live. On a Fast turn with reasoning enabled, an
`EscalationWatch` counts hard tool failures; the **second** one triggers a mid-turn PLAN
and the rest of the turn runs plan-guided, exactly like a Deep turn — replans, surprise
checks and all (the state carries the escalated tier so post-escalation replans keep its
knobs, not Fast's).

| piece | where |
|---|---|
| the watch | `turn/reasoning/mod.rs::EscalationWatch` + `arm_escalation` (armed ONLY when `enabled && auto_escalate && depth == Fast`; toggle off ⇒ no watch object exists at all) |
| the trigger | `ESCALATE_AFTER_HARD_FAILURES = 2`; "hard failure" is **exactly L0's definition** — `surprise::is_tool_failure` (`[error]` / `[tier_blocked]` content prefix, or a structural error envelope), same parse-then-check the plan executor uses. No second notion of failure was invented. |
| the escalation | `maybe_escalate`: a visible `Step(Reasoning)` **stating why** — `"2 hard tool failures — escalating to planning (think)"` with the failure digests as detail — then `Phase(Planning)` + the normal grounding/checklist emissions from the shared PLAN path |
| failure grounding | `PlanInputs.failures` → a `### Hard tool failures this turn (route around these — do not re-plan them)` prompt section ahead of the tool catalog |
| driver seam | `actions.rs`: observe per dispatched tool (only when no plan state exists and a watch is armed — pure counting), escalate at round end; **one-shot** — the watch disarms whether the PLAN succeeds or fail-softs |

## Which tier it escalates to, and why

**`Think` by default** — new config knob `ReasoningConfig.escalate_tier`, clamped to a
planning tier (`Fast` is meaningless there and resolves to `Think`). Rationale: the user
did not ask for deliberation; springing a 20–40 s `think_harder` stall on them mid-turn is
hostile, while `Think` is grammar-guaranteed valid and cost ~5–10 s. A user who wants
escalations to deliberate sets `escalate_tier: "think_harder"` in `reasoning.json`.

## Config default: ON (recommended and shipped)

`auto_escalate` stays default `true`. Justification: (1) it is only reachable behind the
master toggle, which is itself an explicit opt-in default-OFF — a stock install can never
escalate; (2) it only fires when the turn is already going badly (two tools have already
failed hard — the "plain ReAct will probably flail" regime); (3) the cost is one cheap
grammar-guaranteed plan, visible in the InferenceBar with its reason; (4) the identity
carve-out is exactly Aaron's authorized contract, pinned by test. The knob exists for
anyone who wants strict unconditional Fast identity back.

## The narrowed identity proof

New e2e `narrowed_identity_holds_below_the_escalation_threshold`: for BOTH a zero-failure
script and a one-hard-failure script, an enabled+Fast turn (auto_escalate ON) produces an
event transcript and Ollama request bodies **byte-identical** to the toggle-off baseline,
with zero reasoner calls. `auto_escalate_off_never_escalates` additionally pins that the
knob restores unconditional identity even past the threshold (two failures, byte-identical,
zero unary calls). The S3/S4 gate-closed identity test is untouched and green — with
reasoning disabled the contract is still unconditional.

Structurally: below the threshold the only new work on a watched turn is
`is_tool_failure` over each tool reply plus a Vec push on failures — no events, no IPC,
no message changes. An unwatched turn (toggle off) doesn't even construct the watch.

## Escalation behaviour (e2e-pinned)

`second_hard_failure_escalates_fast_turn_to_planning`: two deterministic hard failures
(deferred voice tools → `[error] phase_11_deferred`) → exactly one reasoner call carrying
`think:false` + `num_predict 2048` (the think tier) + the PlanDag grammar + the failures
section naming both tools; the escalation step and `planning` phase are visible; the plan's
guidance rides the next round's tail; the final usage meter folds the plan's tokens.
Fail-soft: a failed/invalid/zero-step escalated plan returns `None` and the turn continues
as plain ReAct (the shared PLAN path's visible fallback notices apply); the watch is spent
either way, so escalation can never fire twice or ping-pong.

## Cost (live, warm default reasoner, ~1,000-token escalated prompt, 3 samples)

| call | wall | tokens | valid |
|---|---|---|---|
| escalated PLAN (think tier, failures block) | **5.8 / 6.7 / 10.8 s** (mean ~7.8 s) | 1,003 prompt + 826–1,148 eval | 3/3 |

Escalation adds **~6–11 s** to a turn that has already burned two failed tool rounds —
statistically the same as an ordinary think-tier PLAN (the failures block costs ~80 prompt
tokens). Qualitative bonus, measured: **3/3 escalated plans routed around the failed tool**
(none re-planned the tool named in the failures block).

## Tests

* harness lib **1154 / 0** (+4: watch counts only L0 shapes / threshold, arm gating,
  `escalate_tier` clamp, failures-section rendering incl. placement before the catalog;
  plus the ordinary-PLAN-has-no-failures-block assertion added to the existing prompt test);
* `reasoning_plan_e2e` **15 / 15** (+3: narrowed identity ×2 scripts, escalation flow,
  knob-off identity);
* `run_turn_loop_e2e` 6/6, `tool_dispatch_e2e` 2/2; fmt clean.

## Push status

`gh auth login` + `setup-git` confirmed working this session — trunk
`feat/thought-bubble-system` pushed to origin (fast-forward, no force; see merge SHA in
the final summary), along with the S4/S4b slice branches. `main` untouched, per Aaron.

## Notes

* Escalation is **Fast-only** by construction (`arm_escalation`): planning tiers already
  have the machinery, and deep→fast never happens — the original S4 done-when criterion
  "escalation fires at most once and never deep→fast" now holds and is pinned.
* No-progress duplicate rounds do NOT count toward escalation (they never reach
  `run_one_tool`); on a plan-guided turn they already have their own L3 trip.
* Nextcloud status board still blocked on the `Nextcloud-AppPassword` credential (same as
  S1–S4 sessions); the plan-doc row carries the status.
