//! Wylde Training panel — gpui-era surface over the
//! `\\.\pipe\wylde-trainer` LLaMA-Factory wrapper.
//!
//! The Tauri/Svelte page (`Core/GUI/src/pages/Training.svelte`) shipped a
//! six-tab studio (Overview, Datasets, Configure, Monitor, Evaluate,
//! Models).  Per the slice-9 scope, the gpui edition focuses on the
//! day-zero loop a user actually walks: see what's training right now,
//! see what trained before, kick off a new run, resume from a past one.
//! Eval, dataset-workshop, and the gallery aren't ported here — they
//! were always reachable from the model-list pages and aren't the
//! "first-launch" surface this slice owns.
//!
//! Surfaces:
//!
//!   * **Active runs strip** — currently-running jobs with progress
//!     (epoch / total, last loss, ETA).  Per-run Cancel button drops
//!     the streaming subscription path and fires the stop verb.  Empty
//!     state ("No active training") is preserved across refreshes.
//!   * **Past runs list** — completed / failed runs with metrics
//!     (final loss, final epoch, duration, base model, dataset).
//!     Selecting a row reveals a bar-histogram loss curve below; the
//!     histogram is rendered with gpui `div` rectangles since the
//!     `b3d93d44` rev has no polyline draw primitive.  See the
//!     externalities note below.
//!   * **Start new training form** — base-model picker + dataset
//!     picker + LoRA params (rank, alpha, dropout) + epochs / batch /
//!     learning rate.  All numeric fields validate inline; Submit calls
//!     the trainer pipe's `start_training` verb and the new run appears
//!     in the active strip on the next refresh tick.
//!   * **Resume from checkpoint** — each past run row carries a
//!     "Resume from this" affordance that pre-fills the start form with
//!     the run's config + checkpoint path.
//!
//! ## Verb discovery + externalities (carry-over from slice 7 / 8)
//!
//! The trainer pipe verbs aren't all live today.  The Python `Trainer/`
//! tree currently ships only captioning; the LLaMA-Factory wrapper that
//! served the Svelte page on port 8013 was retired when the gateway
//! training routes were removed (`Gateway/routes/__init__.py`).  This
//! panel calls the pipe as if the verbs are present — when they aren't,
//! the pipe layer surfaces `pipe_unavailable` / `not_found` and the
//! panel paints its degraded-state rails (empty active strip + dim
//! "trainer surface offline" banner + disabled Submit).
//!
//! Verbs depended on (every one is a punchlist item for the next
//! Trainer slice — none of these block this panel from rendering):
//!
//!   * `GET  /api/jobs`                        — list training jobs
//!   * `POST /api/start_training`              — kick a new run
//!   * `POST /api/jobs/{job_id}/stop`          — cancel a running job
//!   * `GET  /api/jobs/{job_id}/status`        — full status + loss
//!     curve
//!   * `GET  /api/datasets`                    — list LLaMA-Factory
//!     datasets for the form's picker
//!
//! ## Sparkline rendering
//!
//! gpui at `b3d93d44` exposes no polyline / canvas-style draw primitive
//! on a plain `div`.  Two paths were on the table:
//!
//!   * Series of small `paint_quad` rectangles at each sample point
//!     giving a "histogram bars" look — **picked**.  Honest, animates
//!     with the live status reply, and stays within the existing
//!     element vocabulary.
//!   * Textual delta-summary (initial → final loss) with a documented
//!     "sparkline deferred" externality — kept as a fallback when there
//!     are fewer than 2 points but used as the primary inside the
//!     expanded panel only when the histogram would be a single bar.
//!
//! When a polyline primitive lands upstream, the histogram swap is one
//! function (`render_sparkline`); the rest of the panel is unchanged.

pub mod ipc;
pub mod training_panel;

pub use training_panel::TrainingPanel;
