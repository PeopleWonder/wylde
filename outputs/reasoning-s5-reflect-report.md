# Agentic Reasoning S5 — REFLECT: Slice Report

**Date:** 2026-07-14 · **Branch:** `feat/tbs-reasoning-reflect` · **Base:** trunk @ `f47fd71` (post-S4b)
**Plan:** `docs/plans/agentic-reasoning-implementation-plan.md` §5 + §8 S5 (scope P6)

---

## What shipped — the tier is complete

PLAN (S3) grounds, EXECUTE runs guided rounds, surprise+replan (S4) closes the
outcome loop mid-turn. S5 closes the **learning** loop across turns: at the
pre-finalize seam (seam 4), a plan-guided turn's draft answer is critiqued by
one reasoner call, a found gap buys one extra EXECUTE round, and a surviving
**lesson** is written to long-term memory where the very next deep turn's PLAN
reads it back as grounding. PLAN → EXECUTE → REFLECT is now the shipped shape.

| piece | where |
|---|---|
| the critique phase | `turn/reasoning/reflect_phase.rs` (new) |
| the shared tiered call | `plan_phase.rs::tiered_constrained_call` — factored out of `tiered_dag_call` (tier knobs, `num_ctx` cap, grammar, thinking emission, think-exhaustion salvage); PLAN/REPLAN wire bodies pinned unchanged by the existing e2e |
| driver seam 4 | `actions.rs`, inside the natural-completion block, entirely behind `if let Some(reasoning_state)`; critique tokens fold into the turn meter |
| state | `ReasoningState.{surprise_log, reflected}` — surprises recorded by S4's detector feed the critique; `reflected` makes REFLECT once-per-turn by construction |
| prompt | catalog key `"reasoning.critique"` (sibling of `memory.consolidate`) — user-tunable in the Settings prompt editor, no rebuild |
| lesson write | the EXISTING long-term reflection path: `long_term::save` + `REFLECTION_TAG` + `lesson:<kind>` tag + importance floor 7 + `find_duplicate_insight` (τ=0.92) |

**Machinery reused, pass untouched** (the S3-era decision, honored): the
memory-consolidation cycle and its scheduler (`memory/reflection.rs`,
`memory/scheduler.rs`) are unmodified — REFLECT is a turn critique that shares
the prompt catalog and the dedup/supersession write path, not the pass. One
deliberate deviation from the plan's letter: the critique call goes through
`call_reasoner`/`ollama_chat_maybe_constrained` (the S3/S4 pattern) rather
than the `ReflectionChat` trait — the trait returns bare text and can't carry
`format`/`think`/token counts, all of which S5 needs. The trait keeps its one
consumer (consolidation) unchanged.

## The typed lessons record (and its grammar)

```json
{
  "goal_satisfied": true|false,
  "gaps": ["≤3 short strings — concrete unmet parts of the goal"],
  "lesson": {
    "text": "<ONE self-contained transferable sentence>",
    "kind": "tool_behavior" | "planning" | "environment" | "user_preference",
    "confidence": 0.0-1.0
  } | null
}
```

* `reflect_phase::critique_schema()` is the Ollama `format` value, gated on
  the same `constrained_plan` toggle as PLAN and the L2 verdict — the
  constrained.rs policy table's REFLECT row ("only if S5 defines a structured
  lessons record") is now live. **The grammar pins the envelope only** — the
  gap lines and the lesson sentence inside stay free prose (the
  summary-envelope precedent; the never-constrain-prose rule holds).
* Lockstep schema↔serde tests in both directions (minimal admissible object
  deserializes; serialized critique carries every required key; the
  `LessonKind` enum mirrors the wire form) — the only guard on an Ollama build
  that silently fails OPEN on bad schemas.
* `lesson: null` is the documented-correct output for most turns (the
  extractor's empty-lists-are-correct discipline), and `confidence < 0.6`
  (`LESSON_MIN_CONFIDENCE`) is discarded at the store — the model has two
  explicit hedges before a half-guess can pollute the grounding store.
* The stored record's **body is the bare lesson sentence** — PLAN's existing
  `select_lessons` renderer consumes it unchanged; `kind` rides as a
  `lesson:<kind>` tag (filterable later, invisible to the prompt).

## The closed loop — proven end-to-end

`lesson_from_turn_n_grounds_plan_on_turn_n_plus_1` (e2e, mock pipe):

1. **Turn N** (deep, 2 tools): PLAN → 2 guided rounds → draft answer →
   `Phase(Reflecting)` → critique call (grammar-asserted: `format.required ==
   ["goal_satisfied","gaps","lesson"]`, `think:false`, `num_predict 1024`,
   `num_ctx 32768`) returns a lesson → visible `Lesson learned` step with the
   `[tool_behavior] …` detail → the record lands in the long-term store
   (asserted through `select_lessons`, PLAN's actual read surface).
2. **Turn N+1** (deep): the PLAN request body's user prompt contains
   `### Lessons from past sessions` with `- <turn N's lesson>`, and the
   grounding step reports `… 1 lesson(s)` — the lesson is grounding, not
   write-only.

Dedup/supersession: `lesson_learned_twice_reinforces_the_existing_insight`
(unit) pins that a τ-duplicate **touches** the existing insight (recency bump,
zero new records) — reflections cannot pile up near-duplicates; the τ=0.92
decision half is the shared `duplicate_for_vector`, already pinned in the
memory tests.

## Gating policy: `ReflectGate::MultiToolOnly` (default, as planned)

`Off | MultiToolOnly | Always` — persisted knob, S1 shipped it inert, S5 makes
it live. `MultiToolOnly` = plan-guided turn AND ≥2 distinct dispatched calls
(`ToolRoundState.dispatched_hashes`). Justification for keeping the plan's
recommendation over the alternatives the brief floated:

* **vs every deep turn** (`Always`): a single-tool deep turn has almost no
  execution evidence to critique — the draft either answers or it doesn't, and
  the post-turn extractor already covers it. The knob exists for anyone who
  wants it.
* **vs only-on-surprise**: surprises already feed the critique (the
  `surprise_log` block), but gating ON them would miss the other lesson class —
  multi-step work that *succeeded* in an instructive way (tool quirks
  discovered en route). Surprise-gating is a strict subset of what
  MultiToolOnly reflects on.
* Fast turns never reflect — no `ReasoningState`, the seam is unreachable
  (identity). An S4b auto-escalated turn IS plan-guided and does reflect —
  a turn that failed twice and needed a mid-turn plan is exactly a
  lesson-rich turn.
* The gap round: max one per turn, and only when `MAX_TOOL_LOOPS` has rounds
  left (the loop cap stays authoritative, R6). The draft never reaches the
  user; the gap round's answer replaces it.

## Cost — live numbers (dev rig RTX 5080, warm default reasoner UD-IQ3_XXS, constrained, ~695-token critique prompt; 3/2/2 samples)

| tier | REFLECT wall | eval tokens | think chars | valid |
|---|---|---|---|---|
| **think** (the default; `think:false`) | 0.7 / 2.0 / 7.2 s (first-call KV outlier; steady ≈ **0.7–2 s**) | 23–236 | 0 | 3/3 |
| think_harder | 10.2 / 14.6 s | 1,603–2,284 | 6.5k–9.7k | 2/2 |
| ultrathink | 10.2 / 11.0 s | 1,523–1,627 | 6.2k–6.7k | 2/2 |

7/7 grammar-valid. At the recommended default (`think`), **REFLECT adds ~1–2 s
to a multi-tool deep turn** — cheaper than an L2 check plus a replan, and the
verdict/lesson steps make it visibly earn that. The deliberating tiers add
~10–15 s; notably ultrathink's bigger cap buys nothing here — a critique is a
smaller problem than a plan and the model stops ruminating at ~6–7k chars on
its own. A gap round adds one ordinary fast round on top. Worst case stays
bounded: one critique (tier cap + salvage) + one extra round.

## Identity proof (the non-negotiable)

* The gate-closed e2e transcript test now additionally asserts **zero
  `reflecting` phases and zero `Reflection`/`Lesson` steps** in every
  gate-closed transcript (toggle off / Fast), on top of the S3/S4 byte-identity
  over events and request bodies with zero unary calls.
* The S4b narrowed-identity tests are untouched-green: below the escalation
  threshold an enabled+Fast turn is still byte-identical (a Fast turn never
  constructs `ReasoningState`, so the REFLECT seam is structurally
  unreachable — same argument as S4's).
* Fail-soft pinned: `reflection_garbage_never_breaks_the_turn` — an
  unparseable critique emits the visible notice, finalizes the draft verbatim,
  takes no gap round, and still meters the wasted call honestly.

## Tests

* harness lib **1167 / 0** (S4b baseline 1154 + 13: schema↔serde lockstep ×3,
  gate matrix, parse ladder ×2, golden critique prompt, empty-sections +
  abandonment rendering, gap-message rendering, catalog prompt resolution,
  lesson skip/save/reinforce ×3);
* `reasoning_plan_e2e` **19 / 19** (+4: the closed-loop flagship,
  gap-buys-exactly-one-round incl. draft-never-user-facing + one-Reflecting
  proof, gate Off/Always matrix, critique-garbage fail-soft; identity test
  extended; the S4 budget-exhaustion and S4b escalation tests pin
  `reflect_gate: Off` so they stay pure S4 proofs — both would legitimately
  reflect under the default gate now);
* `run_turn_loop_e2e` 6/6, `tool_dispatch_e2e` 2/2, plan crate 21/21; fmt
  clean; clippy: the two new-code hits fixed, remaining reasoning-module
  warnings pre-existing (S4b doc comment, config/residency).

## Notes for Aaron

* **OQ-6 consumed as recommended**: `reflect_gate` default `MultiToolOnly`
  (one-line confirm outstanding, per the handoff protocol).
* The S4 exhaustion / S4b escalation e2e tests now explicitly pin REFLECT off —
  behaviour under the default gate (they *would* critique) is covered by the
  S5 suite instead.
* An abandoned plan (replan budget exhausted) still reflects — arguably the
  most lesson-rich turn; the critique prompt names the abandonment.
* Nextcloud status board still blocked on the `Nextcloud-AppPassword`
  credential (same as S1–S4b sessions); the plan-doc S5 row carries the
  status.
* Measurement script + raw results in the session scratchpad
  (`measure-reflect.ps1`, `reflect-results.json`).

## What's next

S6 — eval + polish (scope §6.2): the fixed multi-step task corpus,
plan-validity/step-success scoring, planted-failure precision/recall, cost
logging off the `Usage` meter, the *Illusion-of-Thinking* easy-task check;
`run_turn` Deep parity and the typed `PlanUpdate` event only if demand
materialises.
