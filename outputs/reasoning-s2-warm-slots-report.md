# Agentic Reasoning S2 — Warm Model Slots: Slice Report

**Date:** 2026-07-14 · **Branch:** `feat/tbs-reasoning-residency` (commit `1f8d74c`, merge `49f76e9`) · **Base:** trunk `feat/thought-bubble-system` @ `8e872c5`
**Plan:** `docs/plans/agentic-reasoning-implementation-plan.md` §8 S2 (scope P3, §6.3a)

---

## The measurement first (the number Aaron asked for)

Nobody had separated MODEL LOAD from THINK time in the S3 latency numbers. Measured
live (dev rig RTX 5080, default reasoner `hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS`,
12.9 GiB resident, real PLAN-shaped calls — grounded prompt + `format` schema +
`num_predict 6144`, seed-paired so cold and warm generate IDENTICAL outputs; Ollama's
own `load_duration` / `prompt_eval_duration` / `eval_duration` give the exact split):

| run | load | prompt ingest | think+generate | wall |
|---|---|---|---|---|
| warm, seed 42 (2157 tok, valid plan) | 0.2 s | 0.14 s | 13.0 s | **13.5 s** |
| cold, seed 42 (identical output) | **4.8 s** | 0.46 s | 13.0 s | **18.3 s** |
| warm, seed 41 (hit the 6144 cap) | 0.2 s | 0.27 s | 37.2 s | **37.7 s** |
| cold, seed 41 (identical output) | **4.6 s** | 0.46 s | 37.3 s | **42.3 s** |

**Verdict: the cold-load penalty is ~4.6 s flat; the S3-reported 19–35 s of deep-turn
planning is ~97% think+generate.** S2 is a real but modest win — it shaves the
eviction penalty, and the think budget (the tiers slice) is the dominant lever.

Also surfaced by the measurement, feeding the tiers slice: at the S3 default budget
(4096 think + 2048 output), **2 of 3 seeds on a grounded prompt ruminated past the
entire 6144-token cap → zero plan JSON → ~37 s wasted, then plain-ReAct fallback.**
The old single-budget default was the worst of both worlds.

## What shipped

Plan §6.3a re-verified against the code before building: `wylde-ollama`'s VRAM leases
are service-internal, per-inference, RAII-dropped (`lease.rs`) — the scope doc's
"harness holds long-lived leases" model still doesn't map onto the code. Warm slots it is.

| piece | where |
|---|---|
| warm-slot loader | `turn/reasoning/residency.rs` — `warm_models` (enabled-gated, deduped, reasoner-first) + `warm_slots_via` (fail-soft, generic over transport, unit-tested without a pipe) + `spawn_warm_slots` (memory-scheduler-style no-runtime guard). One `ollama.preload` per distinct slot model, `keep_alive:"24h"` (the same horizon every chat call already uses). |
| triggers | harness boot (`service::install`) + every successful `settings.reasoning.set` commit (preloading a resident model just refreshes its keep_alive window) |
| embedder unification | `memory/common.rs::embed_model()` — ONE definition: `WYLDE_EMBED_MODEL` env override → `ReasoningConfig.slots.embedder` → default. A fresh install resolves identically to the old env-only path. |
| estimator refinement (the S1.5 fit-chip wart) | `probe_model_sizes` now overlays measured `/api/ps` footprints (`size` = total resident bytes) over the ×1.2 disk estimate for loaded models — with warm slots keeping the defaults resident, the spurious DRAM-offload advisory for the default MoE quant clears. Unloaded models keep the estimate. `fit.rs` pinned-wart test comment updated. |

No broker API change; no new failure mode — an evicted slot degrades exactly like
today (next call reloads: a slow turn, never a failure).

## Identity

`enabled:false` (the default) issues ZERO loads (`warm_models` returns empty; the
spawn declines). Nothing in the slice touches the turn path — residency changes only
*when* a model loads, never what a turn sends. The S3 byte-identity transcript e2e
still passes untouched.

## Tests

harness lib **1138 / 0** (baseline 1129 + 9: warm-set computation incl. Single-mode
dedupe + env override, loader call shape / fail-soft / disabled-zero-loads, measured-size
overlay); `reasoning_plan_e2e` 4/4 (incl. identity), `run_turn_loop_e2e` 6/6,
`tool_dispatch_e2e` 2/2.

**Live check still owed (plan's done-when):** with the toggle enabled on the running
stack, `/api/ps` should show the slot set resident after boot — needs the full stack
up (this session measured the preload verb against raw Ollama, which is the same
`/api/generate {prompt:"", keep_alive}` path `ollama.preload` wraps).

## Notes

* The Nextcloud status board could not be updated from this session (nctool
  credential unavailable — same as S1/S3 sessions); the plan-doc S2 row carries the
  status.
* Measurement scripts + raw responses preserved in the session scratchpad;
  the numbers above are from Ollama's own duration fields, not stopwatch.
