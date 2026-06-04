# Retired: Trainer scope (extracted from Wylde alpha)

**Status:** Extracted 2026-06-04. Cut from the Wylde alpha and set aside as a
separate project to be resumed later as a dedicated `wylde-trainer` repo.

**Last SHA at which trainer code was present in-tree:** `68ef1d1`
(the merge commit this extraction branches from — `git show 68ef1d1`). Every
path listed under [What was removed](#what-was-removed) can be restored from
that commit, e.g.:

```sh
git checkout 68ef1d1 -- Trainer rust/crates/wylde-trainer "Core/GUI/Frontend/Panels/Training"
```

---

## Why deferred

The trainer track depended on a job runner (**LLaMA-Factory**) that was retired.
The five `/api/*` training verbs the gpui Training panel speaks were never
implemented on any service — there is no job runner behind them, and the panel
has been degrading gracefully on their absence (it shows a `pipe_unavailable`
strip rather than erroring). With the job runner gone and no immediate plan to
replace it, the whole track is being deferred rather than carried as dead weight
through the alpha.

The future plan (see memory **`wylde_n8n_principle`** / `[[wylde-trainer-scope-extracted]]`)
is to rebuild the trainer as **thin Rust clients + N8N workflows** rather than a
bespoke in-tree job runner. When that work resumes it will live in its own
`wylde-trainer` project, restored from the git history referenced above.

---

## ⚠️ Note: Caption came with it

The `Trainer/` tree is a "service-of-services": besides the (dead) LLaMA-Factory
training surface it also hosts **Caption** — a *functional* Florence-2-backed
image/video captioning subsystem (`Trainer/Caption/`), its Rust pipe front
(`rust/crates/wylde-trainer`, which serves `caption.*` verbs, **not** training
verbs), and three live harness tools (`caption_image`, `caption_video`,
`caption_batch`, auto-discovered from `Trainer/Caption/tools/`).

Because Caption lives physically under `Trainer/` and shares the `wylde-trainer`
crate and `\\.\pipe\wylde-trainer` pipe name, extracting the Trainer tree per
the alpha-scoping decision **also removed working Caption code**. This was a
clean separation (Caption is not entangled with non-trainer modules), and its
only live surface was already deferred:

- the `visual.caption` verb is a deferred verb (never present in the base prompt);
- the `caption_*` tools were auto-discovered, not hard-wired anywhere.

If Caption needs to come back independently of the training track, restore just
`Trainer/Caption/` + `rust/crates/wylde-trainer` from `68ef1d1` and re-add
`"Trainer"` to the two `SERVICE_ROOTS` lists (see surgical edits below).

---

## The five deferred training verbs

These are the routes the gpui Training panel (`Core/GUI/Frontend/Panels/Training/src/ipc.rs`)
expected a future trainer service to expose over `\\.\pipe\wylde-trainer`. None
were ever implemented. Their expected request/response shapes (from the panel's
client-side projection):

| Verb | Method + path | Request | Response |
|------|---------------|---------|----------|
| list jobs       | `GET  /api/jobs`               | — | `{jobs:[JobRow]}` or bare `[JobRow]` |
| start training  | `POST /api/start_training`     | full `TrainingConfig` (base_model, dataset_name, finetuning_type, lora_rank/alpha/dropout, batch_size, grad_accum, learning_rate, num_epochs, cutoff_len, resume_from_checkpoint?) | `{job_id}` |
| stop a job      | `POST /api/jobs/{id}/stop`     | — | (ignored) |
| job status      | `GET  /api/jobs/{id}/status`   | — | `JobRow` + `{loss_history:[{step,loss,epoch,lr}]}` |
| list datasets   | `GET  /api/datasets`           | — | `{datasets:[{name,display_name,sample_count,format}]}` or bare array |

`JobRow` = `{job_id, status, progress, config, final_loss?, error?, checkpoint_path?, eta_seconds?, duration_seconds?, current_epoch?, total_epochs?}`.
Full struct definitions are recoverable from `ipc.rs` at `68ef1d1`.

The `wylde-trainer` crate as it stood at `68ef1d1` served a *different* verb set
— the Caption surface: `caption.health`, `caption.list_backends`,
`caption.generate`, `caption.generate_batch`, `caption.generate_video`.

---

## What was removed

Total: **~5,466 LOC across 42 files** (3 trees deleted wholesale).

### Deleted trees

| Path | Files | LOC | What it was |
|------|-------|-----|-------------|
| `Trainer/` | 26 | 2,328 | Python: Caption subsystem (Florence-2 captioner, batch/video, 3 tools, `rust_worker.py`) + top-level package shell + tests |
| `rust/crates/wylde-trainer/` | 11 | 719 | Rust pipe front for Caption (`caption.*` verbs → `Trainer/Caption/rust_worker.py`) |
| `Core/GUI/Frontend/Panels/Training/` | 5 | 2,419 | gpui Training panel (`wylde-panel-training`): job list, loss curve, start-run form, dataset picker |

### Surgical edits (references removed; restore alongside the trees)

- `rust/Cargo.toml` — dropped `crates/wylde-trainer` from workspace `members`.
- `Core/GUI/Cargo.toml` — dropped `Frontend/Panels/Training` from `members`.
- `Core/GUI/Manifest/Extension_handlers/`:
  - `Cargo.toml` — dropped `wylde-panel-training` dep.
  - `src/factories.rs` — dropped the `TrainingPanel::view` factory registration + the `default_map_contains_training_panel` test.
  - `src/generated.rs` — regenerated by `wylde-panel-aggregator` (training block gone; do not hand-edit, re-run the aggregator).
- `Core/Lifecycle/`:
  - deleted `daemon_state/_services_trainer.py` (the start/stop pair).
  - `daemon_state/__init__.py` — dropped `_trainer_proc`/`_trainer_worker_proc` slots, the start/stop dispatch entries, and the imports.
  - `daemon_state/_services.py` — dropped the `_services_trainer` re-exports + `__all__` entries.
  - `daemon.py` — dropped the `_start_wylde_trainer{,_worker}` boot calls + comments.
  - `tests/test_reap_manifest_orphans.py` — dropped the two trainer proc slots from the wipe list.
- `rust/crates/wylde-lifecycle/src/`:
  - `state/services.rs` — removed `start_trainer`, `stop_trainer`, `start_trainer_worker`, `stop_trainer_worker`, and the now-unused `spawn_python_script` helper; trimmed trainer mentions from the strangler-table test.
  - `state/mod.rs` — removed the `TRAINER`/`TRAINER_WORKER` service-name consts and their two shutdown-sequence steps (step array `12 → 10`).
  - `daemon.rs` — removed the two trainer boot calls.
- `Core/harness/model_registry/_service_manifests.py` + `rust/crates/wylde-harness/src/model_registry/service_manifests.rs` — dropped `"Trainer"` from `SERVICE_ROOTS` (so the captioner manifest is no longer scanned).
- `Core/harness/tooling/tests/test_smoke.py` — dropped `caption_image`/`caption_video`/`caption_batch` from the expected-tools set.
- `Core/harness/dev/wylde_check/rules/_gpui_contract.py` — dropped the `wylde-trainer` entry from `RUST_SERVICE_REGISTRIES`.
- `Core/shared/system_prompts_catalog.py` — removed the `training` prompt group, the four `TRAINING_*` prompt constants, and their four `PROMPT_CATALOG` entries (the "Training pipeline" personas: dataset_planner, config_reviewer, eval_analyst, pipeline_summary).
- Docs — trimmed trainer rows/notes from `README.md`, `docs/wylde-repo-organization.md`, `docs/deferred-pipe-verbs-2026-05-30.md` (pointer added here).

### Deliberately left in place

- **vram-broker tests** (`rust/crates/wylde-vram-broker/src/{policy,workers}.rs`) use
  `"wylde-trainer"` / `"wylde-caption"` as *example tenant names* for the generic
  VRAM-admission policy. The broker has no trainer dependency; these are fixtures
  and changing them would muddy the broker's own test intent.
- **`VOICE_INTENT_FALLBACK`** in `system_prompts_catalog.py` lists a `Training:`
  voice-intent category. That prompt is Voice scope (a parallel cutover task owns
  it) — left untouched here; its stale `start_training`/`training_status`/`stop_training`
  intents are harmless and should be pruned by the Voice owner if/when desired.
- Dated QA snapshots (`docs/qa/*-2026-05-30.md`, `WYLDE_ENDPOINTS.md`) are
  historical point-in-time records and were not rewritten.

---

## Restoring later

1. `git checkout 68ef1d1 -- Trainer rust/crates/wylde-trainer "Core/GUI/Frontend/Panels/Training"`
   (into the new `wylde-trainer` project, or back in-tree).
2. Re-apply the surgical edits in reverse (re-add workspace members, the panel
   factory, the lifecycle start/stop wiring, `"Trainer"` in `SERVICE_ROOTS`, etc.),
   then re-run `wylde-panel-aggregator` to regenerate `generated.rs`.
3. For the actual training capability, follow `[[wylde-n8n-principle]]`: thin Rust
   clients fronting N8N workflows rather than reviving an in-tree job runner.
