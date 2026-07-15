# Wylde — Benchmark Regression Gate

**The missing third of "trackable, enforceable, benchmarked."** Wylde had
benchmark *harnesses* but no recorded baselines and no regression gate — a
benchmark run by hand and eyeballed is an experiment someone has to remember to
repeat, not enforcement. This directory holds the recorded baselines; the gate
that compares against them is `wylde-release bench` / `wylde-release preflight`
(`tools/wylde-release/`).

> **One-line summary:** `wylde-release bench` runs the eval harnesses against
> live Ollama, compares each metric to a committed baseline with a
> noise-calibrated threshold, **fails on a real regression, warns on a small
> one, and flags an improvement to re-record.** `wylde-release preflight` runs
> it (plus G7) and writes a commit-bound receipt; `wylde-release publish`
> refuses to ship without a green receipt for the exact commit.

---

## What we baseline — and why (opinionated, not everything)

A gate that measures everything gates nothing: noisy metrics train you to ignore
red. So we baseline a *small* set that is meaningful, stable enough to threshold,
and cheap enough to run every preflight — and we justify the exclusions.

| Metric | Source harness | Gate | Why |
|---|---|---|---|
| `reasoning.fast.wall_ms_median` | `reasoning_eval` (fast arm) | **fail** (wide band) | The chat turn latency **users feel** — the single most important number. Wall-clock is noisy, so the band is wide: it catches a *cliff*, not drift. |
| `reasoning.fast.success_rate` | `reasoning_eval` (fast arm) | **fail** | The plain ReAct path still answers correctly. |
| `reasoning.think.success_rate` | `reasoning_eval` (think arm) | **fail** | The reasoning-tier guardrail (roadmap **L5**) — the exact **S6 regression class** (the tier making outcomes *worse*). |
| `reasoning.think.completion_tokens_median` | `reasoning_eval` (think arm) | **fail** (tight-ish) | Token cost is far steadier than wall-clock, so it's the *sharp* cost gate: a jump means the tier genuinely got heavier. |
| `reasoning.think.wall_ms_median` | `reasoning_eval` (think arm) | **warn** | The noisiest number we have (S6 showed ±25%); watched, never gated. |
| `retrieval.lexical.fused_ge_dense` | `lexical_eval` | **fail** (invariant) | Fusion (RRF) must never lose exact-token recall vs dense. **Corpus-independent**, so a hard gate. |
| `retrieval.semantic.fused_ge_dense` | `lexical_eval` | **fail** (invariant) | Fusion must not hurt semantic recall. Corpus-independent. |
| `retrieval.lexical.fused_recall` | `lexical_eval` | **warn** | Absolute recall — **corpus-dependent** (drifts with the live index), so tracked, not gated. |

### Excluded — assessed, and justified

- **Index build time** (`index_bench`) — Ollama-paced *and* corpus-size-
  dependent; the one stable, meaningful part (incremental manifest reuse) is
  better asserted as a unit invariant than gated as a wall number. The harness
  stays; it's a calibration tool, not a gate.
- **Concept-routing quality** (`live_eval`) — the same shape as the lexical
  gate and easy to add later, but it guards **post-0.2** concept-routing work
  (roadmap T3.2) and depends on the live index + decrypted concepts. Wire it in
  when that work lands.
- **Voice encoder latency** (`wylde-voice-bench`) — hardware- and ONNX-artifact-
  specific (needs an exported encoder, optional NPU DLLs); not a headless,
  every-preflight signal.
- **GUI startup / first-paint** — gpui's test platform exposes no queryable
  render tree or paint timing, so first-paint is **not headlessly measurable**.
  It's covered qualitatively by the L2 cold-start smoke and the human feel-test
  (L6), not a number.
- **Memory op cost** — no dedicated harness exists; building one is scope creep
  for this gate. A future addition if save/search latency becomes a concern.
- **Build time** — noisy and machine-dependent; deliberately *not* gated.

---

## The noise/threshold design — the hard part, done honestly

These are **wall-clock measurements on a machine that is also running Ollama on
the GPU.** The S6 eval showed `think_harder` swinging **22–38 s across seeds —
about ±25 % noise.** A gate that fires on that gets muted; a gate loose enough
never to false-fire catches nothing. So:

1. **Median of N reps.** Each reasoning arm runs `reps` times (2 for a
   preflight, 3+ for a baseline you want to trust); we baseline and compare the
   **median**, robust to the odd slow rep in a way the mean is not.
2. **Per-metric bands calibrated to each metric's real variance**, not one
   global threshold — the bands live *in the baseline file next to the number*,
   so a reviewer sees policy and measurement together:
   - **Latency** → *wide* fail band (fast: +40 % fail / +20 % warn). It has to
     clear the ±25 % noise floor to mean anything. This is deliberately coarse:
     a sub-40 % latency regression is **invisible by design**, because the
     alternative is a gate that cries wolf. Latency here catches **cliffs**, not
     drift.
   - **Tokens** → *tighter* band (+30 % fail / +15 % warn). Grammar-constrained
     and budget-capped, so far steadier than wall-clock — this is the sharp cost
     gate.
   - **Success rate** → *absolute points*, not percent (a 0.92 → 0.84 drop is
     "8 points", not "9 %"). Warn at 0.01, fail at 0.10 — one model-refusal
     flake at n≈12 is ~0.08, so it warns; only a multi-task collapse fails.
   - **Retrieval invariants** → boolean, corpus-independent → any break fails.
3. **Two tiers: WARN and FAIL.** Small regressions warn (surfaced, never block);
   only a large regression past the noise floor fails. **Improvements** past the
   fail band are flagged too — they mean the baseline is now pessimistic and
   should be re-recorded.
4. **No fake statistics.** With 2–3 reps a t-test has no power and would lend
   false precision. A noise-calibrated percentage band is both more honest and
   more legible than a p-value nobody can sanity-check.

**Stated plainly so it's never oversold:** the *sharp* gates are the
deterministic ones — the retrieval invariants and success rates. The *latency*
gates are coarse on purpose. Drift below the fail band is what the **committed
trend history** is for (see below), not the pass/fail gate.

---

## Files

- `baselines/wylde-benchmarks.json` — the committed baseline: recorded values,
  the environment they were measured on (GPU / CPU / RAM / model / quant /
  Ollama version), the date, the commit, and each metric's comparison policy.
  **Public** on purpose: useful to contributors, and exposing the perf
  characteristics of an open-source local-first app is low-risk.
- **Trend history** — appended per run to
  `outputs/benchmarks/history.jsonl` (the **private** planning repo, junctioned
  into Core). One JSON line per run: timestamp, commit, green?, every measured
  value. This is the "benchmarks to work from" — drift over time, not just the
  last comparison. It stays private (it's engineering record) and is a silent
  no-op when the junction isn't mounted.

---

## Usage

```bash
# From anywhere in the repo. Runs live Ollama (reasoning) + the live index (lexical).
wylde-release bench                      # compare against the baseline; non-zero exit on FAIL
wylde-release bench --reps 3             # more reps = less noise
wylde-release bench --allow-skips        # don't let an unavailable benchmark block

# Re-record the baseline — the DELIBERATE, explicit path (numbers never drift up
# silently; a regression can only become the new normal on purpose):
wylde-release bench --accept-baseline

# Re-tune thresholds / re-record from a run you already have, without paying the
# LLM cost again:
wylde-release bench --reuse-out --out <dir> [--accept-baseline]

# The full local gate + a commit-bound receipt:
wylde-release preflight                  # G7 + benchmarks (+ --build for L1-lite)
```

### Recording a baseline

`--accept-baseline` re-runs the harnesses and stamps the fresh medians onto the
baseline, **preserving any comparison band you've tuned in the file** (only the
values move, never the policy). It refuses if every benchmark was skipped, so a
baseline is never seeded from a non-run. A degenerate all-zero retrieval result
(a stale/empty index — roadmap T0.5) is treated as a **skip**, not baselined:
baking in a 0.0 would be a permanent false pass.

---

## Honest limitations (what's measured, what isn't)

- **Retrieval is wired but not yet baselined.** The live index on this machine
  is the **move-stale index** (roadmap T0.5): an old Obsidian-vault checkout
  whose corpus doesn't match the gold set's current-tree paths, so both dense
  arms recall 0. The tool detects that and *skips* rather than record garbage.
  Once the current tree is re-indexed (T0.5), `bench --accept-baseline` records
  the retrieval numbers automatically. The **mechanism** is complete and tested;
  the **retrieval data** is blocked on that infrastructure task, and the gold
  set itself is still a draft pending Aaron's vetting.
- **Latency gating is coarse** by design (see the noise section) — it catches
  cliffs, not drift.
- **Two reps is the preflight default** — enough to median-out a single slow
  rep, not enough for tight confidence intervals. Record a baseline with more
  reps (`--reps 3`+) when you want the numbers to carry weight.
