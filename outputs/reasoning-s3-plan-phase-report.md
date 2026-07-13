# Agentic Reasoning S3 — PLAN Phase: Slice Report

**Date:** 2026-07-13 · **Branch:** `feat/tbs-reasoning-plan-phase` (commits `fcd082d` + `b5847c5`) · **Base:** trunk `feat/thought-bubble-system` @ `07c02cc`
**Plan:** `docs/plans/agentic-reasoning-implementation-plan.md` §8 S3 (scope P4) · **Design authority:** `outputs/agentic-reasoning-tier-scope.md` + Aaron's locked decisions (2026-06-26 / 2026-07-13)

---

## What shipped

A Deep turn (`depth:"deep"` + `ReasoningConfig.enabled`) now runs the **PLAN phase**: one
grounded, grammar-constrained reasoner call producing a validated `PlanDag`, whose steps then
**guide** the existing ReAct loop open-loop (steps suggest; outcomes are unchecked until S4's
surprise detector). Everything else — Fast turns, toggle-off, unary `chat.run_turn` — is
byte-identical to trunk, e2e-proven.

| piece | where |
|---|---|
| `route_candidates` exposure | `turn/context_gather.rs` — the turn's routed `CandidateSet` used to be logged-and-dropped inside `gather_with`; it now rides out on `GatheredContext`, so PLAN reuses the turn's own routing. **No second embed, no second route call.** |
| grounded inputs | `turn/reasoning/inputs.rs` — `PlanInputs` + prompt rendering (golden-tested) |
| the reasoner call | `turn/reasoning/plan_phase.rs` — via `constrained::ollama_chat_maybe_constrained` + `plan_dag_format()` (S1.5, ON by default), `num_ctx` capped 32768, parse ladder + validation |
| placeholder resolution | `turn/reasoning/template.rs` — `${sid.output.path}`; type-preserving whole-string splice, textual embedded substitution, unresolved stays verbatim |
| open-loop execution state | `turn/reasoning/mod.rs` — `ReasoningState` (`begin_round` guidance / `finish_round` result recording) + `maybe_plan` (the single driver-facing gate) |
| driver seams | `turn/actions.rs` — depth threaded into `drive_streaming_turn`; seam 1 post-gather (`maybe_plan`), seam 2 round-entry (guidance on the **message tail**, KV-prefix-safe per R5), seam 3 post-dispatch (result recording). Every touch behind `if let Some(state)`. |
| InferenceBar | GUI Chat: `planning`/`replanning`/`reflecting` phase labels (previously fell back to mute "Working"), stage-aware `on_step`, `ActivityKind::Reasoning` (group 3) → a fourth **"Plan"** dropdown section rendering the per-step checklist via the existing `Step` events — zero new widgetry. |

## The grounding (the heart of the slice)

The plan prompt's user message carries, in order (empty blocks omitted):

1. **Goal** — the user message verbatim.
2. **Live concepts** — the turn's activated concepts with settled score + provenance
   (`auth-flow (0.81, seed)`, `token-store (0.66, dependency of auth-flow (1 hop(s)))`).
   Straight from the routed `CandidateSet` — spreading activation is consumed, not reimplemented.
3. **Excluded — NOT relevant** — the IS-NOT scalpel, rendered EXPLICITLY, two faces:
   * concepts the router actually suppressed this turn, with the before→after activation so the
     suppression is *visible*, not silently subtracted:
     `NOT relevant: session-cache — suppressed by exclusion from auth-flow (activation 0.71 → 0.32)`
     (from `Provenance::Inhibited { by, raw }` in the same candidate set);
   * the user-authored negative edges touching any routed concept (activated **or** suppressed —
     a boundary on a suppressed sibling is exactly the disambiguation the planner needs):
     `auth-flow IS NOT oauth-shim — different subsystem`
     (ONE `workspaces.concepts.relations.graph` read, `kind == negative`, dangling excluded).
   The system prompt instructs: *"anything listed as NOT relevant is out of scope — do not plan
   steps into it."* The planner sees what was excluded and why.
4. **Concept boundaries** — per activated concept (≤6), the containment ladder + definition via
   `workspaces.hierarchy.get_node` (`auth-flow → security → architecture — the login path`).
   First `enabled:false` reply short-circuits the block (OQ-9 concepts-only degrade).
5. **Lessons from past sessions** — top-5 long-term records tagged `reflection`,
   importance-then-recency. Read **directly, without the D2 workspace filter** — Aaron's
   authorized relaxation (decision 3), documented in the module doc and `config.rs` so nobody
   "fixes" it back.
6. **Available tools** — the live registry catalog (id + first description line); validation
   canonicalises plan tool names through the executor's own alias map, so the planner can only
   ever name verbs the executor can dispatch.
7. **Context digest** — the turn's rendered `system_slots`, so the planner sees the same
   grounding the executor will.

Every input is fail-soft: unreachable service / disabled toggle / empty store ⇒ empty block;
grounding is additive, never load-bearing (an unbound, routing-off turn plans ungrounded — e2e-covered).

## Fail-soft paths (all → plain ReAct, never a failed turn)

* toggle off / depth Fast → gate never opens (zero cost beyond the existing config-cache read);
* reasoner unreachable → visible `Step(Reasoning)` "Planner unavailable — running direct";
* malformed / schema-failing / unknown-tool / cyclic plan → "Planner output invalid — running
  direct" with the reason as detail;
* zero-step plan (legal "answer directly") → trace kept as Thinking, no guidance;
* mid-turn: the model may deviate from guidance — dispatch authority (salvage net, dedupe, tier,
  consent) is untouched; a round with no tool calls simply completes the turn.

## Honest latency (live, dev rig RTX 5080, default reasoner UD-IQ3_XXS, warm, constrained)

| call | wall | eval |
|---|---|---|
| PLAN (grounded ~950-token prompt), 3 runs | **19.0 s / 23.3 s / 35.2 s** — 3/3 valid | 3.0k–5.7k tok @ 165–166 tok/s |
| fast-turn baseline (same model, direct answer) | **3.7 s** | 533 tok @ 167 tok/s |

A Deep turn costs **roughly +20–35 s of planning** before the first execute round at today's
defaults. The cost is think-dominated (10–19k chars of `message.thinking` per plan), not
prompt-ingestion (953 tokens ≈ 0.2 s) — so R2's "defaults conversation" lever is
`think_budget_tokens`, not the grounding size. Plan tokens fold into the turn's `Usage` meter
(the PLAN call's `prompt_eval_count`/`eval_count` seed `turn_prompt`/`turn_completion`), so the
GUI token meter reports the true deep-turn cost.

**Live finding (fixed in `b5847c5`):** Ollama's `num_predict` caps think + content TOGETHER
(there is no separate think cap), and the reasoner ruminates ~3.5–4k think-tokens on a real
grounded plan prompt — a bare `num_predict = think_budget_tokens (4096)` truncated the plan JSON
mid-string on 1 of 2 warm calls (all 4096 tokens were thinking; zero content). The plan call now
sends `num_predict = think_budget_tokens + PLAN_OUTPUT_BUDGET (2048)`; re-measured 3/3 valid.
Worst case stays bounded: 6144 tok ≈ 37 s at the measured rate.

## Identity proof (the non-negotiable)

1. **e2e transcript test** (`tests/reasoning_plan_e2e.rs::identity_gate_closed_transcript_matches_plain_turn`):
   three full streaming turns over the same mock script — baseline (toggle off, no depth),
   toggle-ON + `depth:fast`, toggle-OFF + `depth:deep`. Asserted: **zero** unary (PLAN) calls;
   the turn-event transcripts byte-equal the baseline; every `ollama.chat_stream` request body's
   `messages` byte-equal the baseline (tool-result contents masked — `time_now` embeds
   wall-clock, an inherent nondeterminism unrelated to reasoning); no `format` key anywhere.
2. **Structural**: every driver-side touch is behind `maybe_plan` (which returns `None` unless
   `deep_gate_open`) or `if let Some(state)`. The only unconditional additions to the fast path:
   `build_alias_map()` moved earlier (pure registry read), one `Option` check per round, and one
   `is_some()` check per dispatched tool. `handle_run_turn` (unary) unchanged: still Fast-only +
   `depth_ignored:true` echo.
3. **Suite**: harness lib 1121/0 — the 1089 baseline plus 32 new, zero regressions; run_turn /
   tool-dispatch e2e binaries green.

## Tests

* harness lib **1121 / 0** (baseline 1089 + 32: inputs rendering incl. the golden plan prompt,
  template resolution matrix, parse/validation ladder, `ReasoningState` walk/placeholder/synthesis
  semantics, GUI-side stage routing);
* `reasoning_plan_e2e` **4 / 4** (deep guided turn, garbage fallback, zero-step, identity);
* `run_turn_loop_e2e` 6/6, `tool_dispatch_e2e` 2/2 (untouched paths still green);
* GUI `wylde-panel-chat` **147 lib** (+2: Plan-section grouping, phase wire mapping) + all
  integration suites; full workspace builds clean.

## Deviations / notes for the next slice

* **`PLAN_OUTPUT_BUDGET`** (above) — a semantic nuance vs the plan doc's "`think_budget_tokens`
  (`num_predict` on the plan call)": the budget now bounds think + output jointly, +2048 for the JSON.
* **fmt churn**: `b5847c5` picked up rustfmt reflow in `constrained.rs`/`fit.rs` (formatting-only).
* **Non-final synthesis steps auto-advance** (a `tool: null` beat mid-plan can't round-trip a
  ReAct round; its narrative rides the next step's guidance; the FINAL synthesis step does guide
  composition). Unit-tested; S4's outcome checks should revisit whether mid-plan synthesis
  deserves its own round.
* **Guidance is append-only** on the message tail (one user-role `[plan step i/n — sid]` block per
  round) — prefix-cache-friendly and the model sees plan progression; S4's replan splice must keep
  this discipline.
* Step results are recorded per round (first result matching the step's tool, else the round's
  first) for `${sid.output…}` — S4's `evaluate()` will consume the same recording.
* Nextcloud status board not updated from this session (nctool credential unavailable — same as
  the planning session); the plan-doc S3 row carries the status.
