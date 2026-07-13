//! `wylde-reasoning-plan` — the isolated, removable data model + pure
//! surprise-detector for Wylde's gated **PLAN → EXECUTE → REFLECT** reasoning
//! tier (agentic-reasoning-tier scope, `outputs/agentic-reasoning-tier-scope.md`).
//!
//! ## What this crate is
//!
//! Just the **types** the planner emits ([`PlanDag`] / [`PlanStep`] /
//! [`ExpectedOutcome`]) plus the **pure** L0/L1 surprise evaluator
//! ([`evaluate`]). No I/O, no Ollama, no Core dependency — it takes a
//! [`serde_json::Value`] tool result in and an [`OutcomeVerdict`] out. This
//! mirrors how `wylde-concept-routing` / `wylde-concept-hierarchy` are isolable:
//! the novel logic lives in a pure library the experiment can be deleted by
//! dropping, without touching the turn path.
//!
//! ## Phase status (scope §6.1)
//!
//! * **P0 (this slice)** — the data model ([`model`]) + the pure `evaluate()`
//!   ([`evaluate`]) covering L0 (deterministic: errored/empty) and L1 (declared
//!   structural predicates). Assertion-only steps are allowed: a clean step uses
//!   predicates (L1); a fuzzy one uses an `assertion` + low `confidence` to flag
//!   an L2 fast-model check.
//! * **Constrained-decoding slice (2026-07-13)** — [`schema::plan_dag_format`],
//!   the canonical JSON Schema handed to Ollama's `format` parameter. This is
//!   the crate's first harness consumer (`turn/reasoning/constrained.rs`), so
//!   the crate is now *build-linked* but still **runtime-inert**: nothing
//!   reaches it unless `ReasoningConfig.enabled` (default OFF) opens the deep
//!   gate. The off ⇒ identity contract is unchanged.
//! * **P1+** — the gated turn wiring (`<think>` events, the InferenceBar
//!   fast/deep + Split/Single controls, `ReasoningConfig`, the PLAN call, the
//!   L2/L3 surprise layers, REFLECT). All land behind `ReasoningConfig.enabled`
//!   + the per-turn `depth` flag in later phases.
//!
//! ## Isolation / identity contract (scope §0)
//!
//! At P0 the crate is purely additive: no existing crate depends on it, so the
//! everyday fast-model ReAct path is **byte-identical** to today. The data-level
//! analogue of the off ⇒ identity rule is [`ExpectedOutcome::trusting`], for
//! which `evaluate` always returns a never-surprising, no-L2 verdict.

#![forbid(unsafe_code)]

pub mod evaluate;
pub mod model;
pub mod schema;

pub use evaluate::{evaluate, is_empty_value, is_error_envelope, L2_CONFIDENCE_THRESHOLD};
pub use model::{
    ExpectedOutcome, OutcomePredicate, OutcomeVerdict, PlanDag, PlanStep, SurpriseAction,
};
pub use schema::plan_dag_format;
