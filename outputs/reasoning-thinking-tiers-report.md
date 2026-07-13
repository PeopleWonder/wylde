# Agentic Reasoning — Thinking Tiers: Slice Report

**Date:** 2026-07-14 · **Branch:** `feat/tbs-reasoning-tiers` (commit `171a51d`) · **Base:** trunk @ `49f76e9` (post-S2)
**Companion:** `outputs/reasoning-s2-warm-slots-report.md` (the cold-vs-warm breakdown that reordered this work)

---

## What shipped

The Fast/Deep pill is now a **tier ladder**, explicitly modelled on Claude's
think / think-harder / ultrathink levels. Each tier is one per-call knob pair on the
PLAN call — a `think` switch and a `num_predict` budget — everything else (grounding,
grammar constraint, fail-soft ladder, seams) is unchanged.

| tier | wire token | reasoner `<think>` | think budget | `num_predict` |
|---|---|---|---|---|
| Fast | `fast` | — (no PLAN call) | — | — |
| Think | `think` | **disabled** (`think:false`) | 0 | 2048 (output allowance only) |
| Think harder | `think_harder` (legacy `deep` maps here) | on | 4096 | 6144 |
| Ultrathink | `ultrathink` | on | 10240 | 12288 |

Wired through: `ReasoningConfig.tier_budgets{think_harder,ultrathink}` in
`reasoning.json` (replaces the single `think_budget_tokens`), the `depth` wire field
(`Depth::parse` accepts the four tokens + legacy `"deep"`), and the InferenceBar pill
(cycles the ladder, humanized labels). `settings.reasoning.set`'s partial-patch merge
covers the new knobs with no verb change.

## The two live findings that dictated the shape (measure first paid off)

1. **A tight think CAP is an empty-plan machine.** Ollama's `num_predict` caps
   think + content TOGETHER, and a generation that hits the cap mid-`<think>`
   produces **zero content** — the grammar constrains `message.content` only and
   cannot force the model out of the think channel (probed live: `think:true` +
   2048 cap → 8306 chars of thinking, 0 content, `done_reason:"length"`). The
   task brief's hope that "a truncated think stream still yields schema-valid
   JSON" is **refuted**. So the tight tier disables deliberation instead of
   capping it: `think:false` → the grammar guarantees the JSON, ~4 s.
   (`think:"low"` was also probed: accepted by the API but a silent no-op on
   qwen — full rumination anyway. Unusable.)
2. **The old default was the worst of both worlds.** At the S3 default
   (4096+2048), 2 of 3 seeds on a heavy grounded prompt ruminated past the
   ENTIRE 6144-token cap: ~37 s spent, no plan at all, silent fallback to plain
   ReAct. (The S3 report's 3/3-valid was a luckier draw; rumination variance is
   large.)

**Think-exhaustion salvage (new):** when a deliberating tier dies at
`done_reason:"length"` with zero content, the plan phase now retries ONCE with
`think:false` on the output allowance alone — visible Step
("Deliberation used the whole budget — retrying without it"), both calls' tokens on
the meter. Worst case becomes tier-cap + ~4 s with a valid (unthought) plan instead
of tier-cap wasted with nothing.

Fail-soft additions: deliberating tiers OMIT the `think` field entirely (a
non-thinking reasoner keeps working exactly as in S3 — Ollama rejects an explicit
`think` on models without the capability); if a backend rejects the `Think` tier's
`think:false`, one retry drops the field (mirrors the S1.5 format retry).

## The tier curve (the table Aaron asked for)

15 grounded PlanDag prompts × 3 tiers, live default reasoner
(UD-IQ3_XXS, warm, constrained decoding, seed 7; validity = full structural
PlanDag check; tool-hallucination checked against each prompt's catalog):

| tier | schema-valid | real tools | mean wall | max wall | mean steps | empty `args_template` | notes |
|---|---|---|---|---|---|---|---|
| **think** | **15/15** | 14/15 (one `fs.stat` hallucination — harness validation rejects that plan safely to ReAct) | **4.0 s** | 5.2 s | 3.0 | 21/37 tool steps | plans are correct OUTLINES; executor fills args (by design — guidance, not authority) |
| **think_harder** | 13/15 — both failures were rumination past the cap, `done_reason:"length"`, zero content; **the shipped salvage converts these to valid at +~4 s** | 13/13 | 22.0 s | 38 s | 2.5 | 4/26 | concrete args + `${sN.output…}` chains |
| **ultrathink** | **15/15** | 15/15 | 24.5 s | **65.3 s** | 2.6 | 4/31 | catches the rumination tail; identical output to think_harder whenever rumination fits both budgets |

Caveat for honesty: this corpus grounds each prompt with a 12-verb catalog only.
The *heavier* fully-grounded prompt used in the cold/warm measurement (concepts +
exclusions + ladders + digest) overran the think_harder cap on 2 of 3 seeds —
richer grounding provokes more rumination, so real workspace-bound deep turns
will lean on the salvage more often than 2/15 suggests.

What deliberation buys, measured: concrete arguments (empty args 57% → 15%),
placeholder chaining (6/15 → 8–9/15 plans), zero tool hallucinations, slightly
tighter plans. What it costs: 5–6× latency and a salvage-managed overrun risk.

## Recommended default — `think` (deliberation off), and here is the evidence

* `default_depth` stays **Fast** (locked: never planning-by-default).
* The default *planning* tier — the pill's first stop, what a user opting into
  reasoning gets — is **Think**: 100% grammar-valid plans at **~4 s** (vs 3.7 s
  for a plain fast turn — planning becomes nearly free), 5–6× faster than the
  old 4096-budget Deep, immune by construction to the empty-plan failure mode
  that burned 37 s at the old default. Its weaker args are tolerated by the
  S3 execution design (the fast model writes the real call; suggestions may be
  wrong). Effective think-budget default therefore drops 4096 → **0**, with
  deliberation an explicit escalation: `think_harder` (4096) when the user wants
  reasoned structure, `ultrathink` (10240) when they want the tail covered.
* Legacy `"deep"` keeps meaning think_harder, so nothing that shipped changes
  meaning.

## Identity proof

* `identity_gate_closed_transcript_matches_plain_turn` (unchanged, green): toggle
  OFF / depth `fast` ⇒ event transcripts + every Ollama request body byte-identical
  to baseline, zero PLAN calls, zero `format` keys.
* Structural: `Fast.plans() == false` short-circuits the gate exactly as before;
  the only semantic change below the gate is *which knobs ride the PLAN call* —
  fast-path bodies are untouched.
* Legacy-wire test pins `"deep"` → think_harder.

## Tests

* harness lib **1140 / 0** (S2's 1138 + net 2: tier ladder/budgets/legacy-parse);
* `reasoning_plan_e2e` **6 / 6** (+2: `think_and_ultrathink_tiers_set_the_call_knobs`
  — think:false + 2048 on the wire for Think, no think key + 6144/12288 for the
  deliberating tiers; `think_exhaustion_salvages_grammar_first` — exactly one
  salvage retry, think:false + 2048, visible notice, honest token meter);
* `run_turn_loop_e2e` 6/6, `tool_dispatch_e2e` 2/2;
* GUI `wylde-panel-chat` **148 lib** (+1 net: 4-way cycle, wire tokens, labels) +
  all integration suites.

## Notes / deviations

* An existing `reasoning.json` carrying the old `think_budget_tokens` key reads
  as the new defaults (tolerant loader ignores unknown keys) — acceptable: the
  feature is default-off and unreleased; noted in the config docs.
* `run_turn` (unary) unchanged: any planning tier echoes `depth_ignored:true`.
* Eval scripts + all 45 raw plans preserved in the session scratchpad;
  `tier_results.json` has the per-row data.
* Nextcloud status board not updated (nctool credential unavailable — same as
  S1/S2/S3 sessions).
