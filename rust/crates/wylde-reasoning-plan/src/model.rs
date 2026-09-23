//! The planner's data model — the plan/tool DAG and the expected-outcome
//! schema that is the **surprise-detection key**.
//!
//! These types are pure data (`serde`-(de)serialisable, `Core`-free). A Deep
//! turn's reasoner emits a [`PlanDag`] in one pass; EXECUTE realises it round
//! by round on the fast model. Each [`PlanStep`] carries an [`ExpectedOutcome`]
//! declaring — *at plan time* — what a non-surprising result looks like, so the
//! surprise detector can fire with zero model calls at L0/L1 (see
//! [`crate::evaluate`]).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The planner's whole output for a Deep turn.
///
/// ReWOO-flavoured: the reasoner emits the entire step graph in one pass, with
/// placeholders for not-yet-known results. EXECUTE resolves those placeholders
/// and dispatches each step on the fast model, round by round.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanDag {
    /// Restated user intent. Grounds REFLECT's self-critique.
    pub goal: String,
    /// The steps. Topological-ish for readability, but [`PlanStep::depends_on`]
    /// is the authoritative edge set.
    pub steps: Vec<PlanStep>,
    /// The reasoner's `<think>` block, surfaced to the thought-bubble UI.
    pub reasoning_trace: String,
    /// Bumped on each replan (`1` = the first plan).
    pub plan_version: u32,
}

/// One node in a [`PlanDag`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Stable id within a `plan_version` — e.g. `"s1"`, `"s2"`.
    pub id: String,
    /// Human-readable goal of this step.
    pub intent: String,
    /// The verb name to dispatch. `None` = a pure reason/synthesis beat that
    /// produces no tool call, only advances the narrative.
    pub tool: Option<String>,
    /// Args with ReWOO-style `${stepid.output.jsonpath}` placeholders, resolved
    /// from prior step results before dispatch.
    pub args_template: Value,
    /// DAG edges — ids of steps that must complete first.
    pub depends_on: Vec<String>,
    /// The surprise key: what a non-surprising result looks like.
    pub expected: ExpectedOutcome,
}

/// What a step declares, at plan time, as a non-surprising result.
///
/// The [`predicates`](Self::predicates) are cheap declarative checks evaluated
/// with **zero model calls** (L1). The [`assertion`](Self::assertion) is the
/// single natural-language yes/no handed to the fast model only when L1 is
/// inconclusive (L2). [`confidence`](Self::confidence) lets the planner force
/// an L2 check even when L1 passes, to catch plausible-but-wrong tool output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpectedOutcome {
    /// L1 — dumb structural/string predicates evaluated against the tool
    /// result with zero model calls. **All** must hold or the step is
    /// "surprising".
    pub predicates: Vec<OutcomePredicate>,

    /// L2 — a single yes/no the fast model answers only when L1 is ambiguous
    /// (no predicates, or predicates passed but the planner flagged it fuzzy
    /// via low [`confidence`](Self::confidence)). Empty ⇒ no L2 check possible.
    pub assertion: String,

    /// What to do when this step surprises. Default [`SurpriseAction::Replan`].
    pub on_surprise: SurpriseAction,

    /// Planner confidence `0.0..=1.0`. Low confidence biases toward an L2 check
    /// even when L1 passes (the plausible-but-wrong case).
    pub confidence: f32,
}

impl ExpectedOutcome {
    /// An expectation that asserts nothing: no predicates, no assertion, full
    /// confidence. [`crate::evaluate`] returns a never-surprising, no-L2
    /// verdict for it — the data-level analogue of the off ⇒ identity contract
    /// (a fully-trusted planner step).
    pub fn trusting() -> Self {
        Self {
            predicates: Vec::new(),
            assertion: String::new(),
            on_surprise: SurpriseAction::Continue,
            confidence: 1.0,
        }
    }
}

/// A single declarative L1 check against a tool result.
///
/// Path-bearing variants use [RFC 6901](https://datatracker.ietf.org/doc/html/rfc6901)
/// JSON Pointers (`""` = the whole document, `"/entries/0/name"` = nested),
/// matching [`serde_json::Value::pointer`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomePredicate {
    /// Result is non-null and not an empty array/string/object.
    NonEmpty,
    /// The JSON-Pointer `path` resolves to a value.
    JsonPathExists { path: String },
    /// `path` resolves and equals `value`.
    JsonPathEquals { path: String, value: Value },
    /// The serialised result contains `needle` (case-insensitive when `ci`).
    Contains { needle: String, ci: bool },
    /// The array at `path` has length `>= n`. Fails if `path` is absent or not
    /// an array.
    CountAtLeast { path: String, n: usize },
    /// Result is **not** a tool-error envelope (belt-and-braces over L0).
    NoError,
}

/// What to do when a step's result surprises the detector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurpriseAction {
    /// Hand the surprise back to the reasoner (budget-gated replan).
    Replan,
    /// Log it and keep going (a soft expectation).
    Continue,
    /// Unrecoverable precondition — end the turn cleanly.
    Abort,
}

impl Default for SurpriseAction {
    /// Default is [`SurpriseAction::Replan`] — the scope's declared default.
    fn default() -> Self {
        SurpriseAction::Replan
    }
}

/// The result of [`crate::evaluate`] — the L0/L1 verdict for one step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutcomeVerdict {
    /// L1 found a definitive mismatch (one or more predicates failed).
    pub surprised: bool,
    /// The predicates that failed — fed to the replan prompt and the bubbles.
    pub failed_predicates: Vec<OutcomePredicate>,
    /// L1 was inconclusive (or low-confidence): ask the fast model the
    /// [`ExpectedOutcome::assertion`] at L2. Never set when `surprised` is true
    /// or when there is no assertion to ask.
    pub needs_l2: bool,
}
