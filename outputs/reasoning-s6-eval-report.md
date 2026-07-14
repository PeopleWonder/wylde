# Agentic Reasoning S6 — Outcome Eval: Slice Report

**Date:** 2026-07-14 · **Branch:** `feat/tbs-reasoning-eval` · **Base:** trunk @ `9a1a087` (post-S5)
**Plan:** `docs/plans/agentic-reasoning-implementation-plan.md` §8 S6 (scope §6.2)
**Harness:** `rust/crates/wylde-harness/examples/reasoning_eval.rs` (committed, re-runnable)

---

## The question S6 exists to answer

Every prior reasoning slice (S0–S5) measured **latency, JSON validity, and
byte identity**. None measured the only thing that ultimately matters:
**does the PLAN→EXECUTE→REFLECT tier produce better ANSWERS than the plain
ReAct loop, or does it just cost latency?** S6 answers that honestly, with
numbers, including the negative findings.

## How the eval is built (and why it's trustworthy)

A committed example binary drives the **real streaming turn driver**
(`handle_start_turn` + `handle_stream_turn`) over a fixed task corpus, once
per arm, against the **live default reasoner** (`Qwen3.6-35B-A3B UD-IQ3_XXS`)
on Ollama.

* **Real turn path, real tools.** Not a mock. The turn runs through the
  actual gate, PLAN phase, grammar-constrained reasoner call, surprise
  detector, reflect gate, salvage net, tier/consent gates, and the real
  shipped tool catalog (the eight `wylde_*` verbs over `time` / `fs_file` /
  `fs_dir` / `graph` / …).
* **Hermetic + reproducible.** Only Ollama must be up. The eval stands up
  its own `PipeServer` that registers `ollama.chat` / `ollama.chat_stream` /
  `ollama.embed` and **proxies them faithfully to Ollama HTTP** — dropping
  the pipe-only `priority`, forcing `stream`, injecting a fixed seed, and
  forwarding `model`/`messages`/`tools`/`format`/`think`/`options` verbatim,
  exactly as `wylde-ollama` does. The workspaces service is a dead pipe (the
  driver degrades fail-soft); no daemon, broker, or Memgraph needed.
* **Re-runnable regression harness:**
  ```
  cargo run --release --example reasoning_eval -- \
      --arms fast,think,think_harder,ultrathink --reps 2 --out outputs
  cargo run --release --example reasoning_eval -- --smoke      # 4-turn sanity
  cargo run --release --example reasoning_eval -- --reflect    # cross-task learning
  ```
  Raw per-run rows land in `outputs/reasoning-eval-results.json`; the
  aggregate tables in `outputs/reasoning-eval-results.md`.

### Arms

| arm | config | what it is |
|---|---|---|
| `fast` | reasoning OFF | plain ReAct — **the control** |
| `fast_auto` | enabled, Fast tier, `auto_escalate` on | S4b: escalates to planning only after 2 hard tool failures |
| `think` | enabled, `think` tier | PLAN on, deliberation off (grammar-first) — the shipped default planning tier |
| `think_harder` | enabled, `think_harder` tier | PLAN on, 4096 think budget |
| `ultrathink` | enabled, `ultrathink` tier | PLAN on, 10240 think budget |

Same seed per (task, rep) across arms. `reflect_gate` is **Off** in the arm
sweep so it measures the PLAN + surprise/replan/escalate machinery in
isolation; REFLECT is measured in its own controlled experiment.

### Corpus (6 tasks, programmatic success criteria)

| id | category | goal | success = answer contains |
|---|---|---|---|
| `A1_time` | simple | read the system clock via a tool | the live year (`2026`) |
| `A2_read` | simple | read a file, report line 1 | `WYLDEROOT` |
| `B1_dep_chain` | multi-step | config field → derive filename → read → report function | `compute_invoice_total` |
| `B2_search` | multi-step | find the file containing a marker | `billing.py` |
| `B3_count` | multi-step | list a dir, count `.py` files | `2` |
| `C1_graph_recover` | recovery | try the (dead) graph, fall back to a glossary file | `0.0875` |

Simple tasks are the **Illusion-of-Thinking probe**: Deep must not lose on
easy tasks. Multi-step tasks are where a plan plausibly helps. The recovery
task is a soft failure (see the honest limitation below).

### Metrics

Per run: **task success** (needle match on the final answer, programmatic);
**tool efficiency** (dispatched tool count, reconstructed from the execution
rounds' message history); **recovery** (planted-tool failure → routed
around); **latency** (wall-clock) and **token cost** (the `Usage` meter,
which folds the reasoner PLAN/REPLAN/REFLECT tokens into the turn total);
plus flags for `planned` / `replanned` / `escalated` / `aborted`.

---

## The headline finding: a shipped fail-soft REGRESSION (found, diagnosed, fixed)

The first thing the eval found is not a latency curve — it's a **bug that
was live in the shipped reasoning tier** and that every prior identity/e2e
test missed.

**Symptom.** On multi-step tasks — including `B1_dep_chain`, *exactly* the
dependency-chain shape where a plan should win — the planning arms (`think`
/ `think_harder` / `ultrathink`) produced **0 tools dispatched, an abort,
and an empty answer**, while plain-ReAct `fast` succeeded cheaply.

**Diagnosis (from a dumped failing run, not a guess).** The `think` plan for
`B1_dep_chain` was well-formed:

```
s1  tool=read_file   expects /active_module exists   on_surprise: ABORT
s2  tool=read_file   (after s1)                       on_surprise: ABORT
s3  synthesis                                          on_surprise: continue
```

The executor's **first** call was `wylde_describe` — because the verb
catalog *requires* a discovery call before any resource op (resource types
aren't in the always-on prompt). The abort event was decisive:

```
s1 surprised: 1 expected check(s) failed
  expected: path /active_module exists
  actual:   {"count":12,"resources":[… the wylde_describe catalog …]}
ABORTED reason="plan_precondition"
```

**Root cause.** `ReasoningState::finish_round` bound the in-flight step to
the round's result with `…find(step.tool).or_else(|| round_results.first())`.
When the step's own tool wasn't called that round, it fell back to the
**first** result — the unrelated `wylde_describe` catalog — checked `s1`'s
`/active_module` predicate against it, "surprised", and because the planner
had set `on_surprise: abort`, **killed the turn with an empty answer.** A
second, deeper contributor: the planner is grounded in the *full* registry
(so it names `read_file`) while the executor is only advertised the eight
`wylde_*` verbs (so it calls `wylde_get`) — the plan's tool names can never
match what the model dispatches.

**Is the fail-soft guarantee broken?** *Yes, in spirit.* Every slice
promised "a planner failure degrades to plain ReAct, never breaks the turn."
That covers *malformed* plans. It did **not** cover a *valid* plan whose
execution-time outcome check aborts the turn to empty — which is strictly
worse than the ReAct control. The tier was making outcomes worse, not just
slower.

**Why every test missed it.** A genuine coverage gap: **every** S3/S4/S5
e2e test has the mock model call the step's *declared* tool (`time.now`),
so `finish_round` always matched by tool and the `or_else(first())` fallback
never fired. No test exercised "the executor dispatches a tool the plan step
didn't name" — the *normal* case under the verb catalog. Identity/JSON-
validity/latency tests can't catch an outcome regression by construction.

**The fix** (`turn/reasoning/mod.rs` + `turn/actions.rs`, this branch):
1. `finish_round` now binds a step's result **only to a dispatch of that
   step's own tool** (canonicalised). If the round dispatched a different
   tool (discovery / a different verb), the step is *not* realised: it stays
   in flight, its expectation is **not** evaluated, and no-progress is not
   tripped. Guidance is advisory by design (OQ-3), so this is correct. The
   loop cap remains the safety net.
2. Round results are recorded under their **canonical** tool id (the plan
   stores canonical ids; the model emits dotted/aliased names) so a genuine
   match isn't missed.
3. New regression e2e `unplanned_discovery_call_does_not_false_abort_the_step`
   pins the exact case; all prior e2e stay green (an abort still fires when
   the step's *own* tool runs and truly fails — `abort_action_ends_the_turn_
   cleanly` is unchanged).

Tests after the fix: harness lib **1167/0**, `reasoning_plan_e2e` **20/20**
(+1 regression).

**Before → after (the same `B1_dep_chain` on `think`):**

| | tools | abort | answer |
|---|---|---|---|
| before fix | 0 | `plan_precondition` | *(empty)* — ❌ |
| after fix | 3 (`wylde_describe`,`wylde_get`,`wylde_get`) | none | `compute_invoice_total` — ✅ |

---

## RESULTS (after-fix live run — `Qwen3.6-35B-A3B UD-IQ3_XXS`, 6 tasks × 2 reps)

Raw rows: `outputs/reasoning-eval-results.json`. Aggregate: `outputs/reasoning-eval-results.md`.

### Per-arm aggregate (12 runs each)

| arm | success | median wall | median tools | median compl-tokens | reasoner calls | total wall | total compl-tokens |
|---|---|---|---|---|---|---|---|
| `fast` (control) | **11/12 (92%)** | **8.4 s** | 3 | 506 | 0 | 214 s | 6,760 |
| `fast_auto` | **12/12 (100%)** | 8.9 s | 3 | 540 | 0 | 197 s | 6,863 |
| `think` | **12/12 (100%)** | 31.5 s | 3 | 1,365 | 1 | 470 s | 17,591 |
| `think_harder` | **10/12 (83%)** | 43.8 s | 3 | 2,675 | 1 | 713 s | 27,040 |
| `ultrathink` | **10/12 (83%)** | 44.0 s | 3 | 2,675 | 1 | 714 s | 27,205 |

### Success by category × arm

| category (n each) | fast | fast_auto | think | think_harder | ultrathink |
|---|---|---|---|---|---|
| simple (4) | 4/4 | 4/4 | 4/4 | 4/4 | 4/4 |
| multi-step (6) | 5/6 | 6/6 | 6/6 | **4/6** | **4/6** |
| recovery (2) | 2/2 | 2/2 | 2/2 | 2/2 | 2/2 |

All 4 remaining failures are **`B2_search` timeouts on the deliberating tiers**
(`think_harder`/`ultrathink`, both reps hit the 150 s cap with an empty answer).
The one `fast` miss was a **model refusal** on `B3_count` rep 0 ("I don't have
the ability to access local files") — a generation flake, not something planning
fixed (every other non-refusing run, planning or not, got it).

### Reflection experiment (teach → apply, `reflect_gate: Always`, `think`)

The teach turn ran REFLECT (`reflected=true`) but the critique returned
**`lesson: null`** — no transferable lesson stored. So apply-with-lesson and
apply-clean-store are statistically identical (both succeed, 48–52 s, 3–4 tools,
0 graph attempts). **No measurable cross-task benefit — because the reasoner
emitted nothing to transfer.** The closed loop is proven MECHANICALLY (the S5
`lesson_from_turn_n_grounds_plan_on_turn_n_plus_1` e2e), but organically, on a
task that simply succeeded, it produced no lesson — and `null` is the documented-
common critique output. Reflection's practical value here is **unmeasured**: it
fires but seldom emits anything to learn from.

---

## VERDICT — does the reasoning tier improve outcomes? **No (on this corpus).**

Numbers, not vibes:

1. **Success: planning does not beat plain ReAct.** The best planning tier
   (`think`) ties the controls at 100%; the `fast` control's single miss was a
   refusal flake, not a planning-addressable gap (n=2 is too small to call a
   real win). **Heavier deliberation is strictly WORSE** — `think_harder` and
   `ultrathink` drop to 83% by timing out on the search task where `fast`
   succeeds.
2. **Cost: multiples, for nothing.** `think` costs **3.7× the wall-clock and
   2.6× the tokens** of `fast` for the same answers; the deliberating tiers cost
   **~3.3× wall / 4× tokens** for *lower* success. The *Illusion-of-Thinking*
   signal is confirmed: more deliberation → equal-or-worse accuracy at
   multiplied latency.
3. **Tool efficiency: unchanged.** Median 3 tool calls across *every* arm —
   planning does not reduce or better-order tool use here.
4. **Recovery / surprise / replan / escalate: never organically fired.**
   Degraded services return empty-ok (not `[error]`), and deferred `[error]`
   tools aren't advertised, so no hard L0 failure arises from real task
   execution. `fast_auto` never escalated (`reasoner_calls=0` on every run). All
   arms handled the recovery task via the instructed file fallback. The
   machinery is proven only mechanically (S4/S4b e2e).
5. **Reflection: fired, emitted nothing, changed nothing** (see above).

**The one unambiguous S6 win:** the eval *found and fixed a shipped bug that was
making planning strictly worse than ReAct* — empty-abort turns on exactly the
multi-step tasks planning was meant to help. Before the fix, `think`/`think_harder`/
`ultrathink` empty-aborted on `A1_time` and `B1_dep_chain`; after, they complete
correctly. That regression shipped through a full green e2e/identity suite — a
finding about our **test coverage** (no test exercised the executor calling a
tool the plan step didn't name) as much as about the tier.

### So what should happen to the tier?

- **Keep it OFF by default** (it already is) — the honest posture.
- The tier as-shipped, even bug-fixed, is a **latency tax with no measured
  benefit** on hermetic tasks. Its plausible value lives in the one thing this
  eval deliberately could not test: **grounding** (concept routing / exclusions /
  lessons over a real workspace). If planning is to earn its cost, that is where
  to prove it — not the planning machinery, which is now correct but inert.
- **Fix the planner↔executor tool-vocabulary mismatch** (planner is grounded in
  the full registry and names `read_file`; executor is advertised only the
  `wylde_*` verbs and calls `wylde_get`). Today plans can never guide execution
  by tool identity — the fix makes that non-fatal, but a plan whose suggestions
  the executor can't follow is close to useless. Grounding the planner in the
  *advertised* catalog is the highest-value follow-up.
- **`think` is the only defensible planning tier**; `think_harder`/`ultrathink`
  regressed on both success and cost here — reserve them, don't default to them.

---

## Honest limitations (read before trusting a number)

1. **Grounding is DEGRADED.** Concept routing, hierarchy, and workspace RAG
   need the workspaces service, which is off in the hermetic harness. This
   eval therefore isolates the value of the **planning machinery**, not the
   value of the grounding the plan phase consumes. A plan grounded in a real
   workspace's concepts/exclusions/lessons could do better than these
   numbers; that is untested here and is the single biggest caveat.
2. **Organic HARD tool failures essentially don't occur.** The `[error]`
   L0 failures that drive surprise/replan/escalate come from tools the
   model isn't advertised (deferred tools are filtered out of the catalog),
   and degraded services (the down `graph`/Memgraph) return **empty-ok, not
   an error envelope**. So the surprise/replan/escalate machinery rarely
   fires from real task execution here. It is proven MECHANICALLY by the
   `reasoning_plan_e2e` planted-failure suite (S4/S4b); this eval measures
   how the tier behaves on organic tasks, where the notable failure mode is
   an over-strict expected-outcome **aborting** a turn (see results).
3. **One local reasoner, one quant.** All numbers are the shipped default
   (`Qwen3.6-35B-A3B UD-IQ3_XXS`, Single mode — plan and execute on the same
   model). A stronger reasoner in Split mode is untested.
4. **Small n.** Reps are limited by wall-clock (each deliberating-tier turn
   is tens of seconds). Treat the tables as directional, not significant to
   many digits; the harness is built to re-run at higher n.
