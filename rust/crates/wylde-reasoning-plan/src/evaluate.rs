//! The pure surprise evaluator — **L0 (deterministic) + L1 (declared
//! structural)**, with zero model calls.
//!
//! [`evaluate`] is the heart of the surprise-detection key: given a step's
//! [`ExpectedOutcome`] and the realised tool result, it returns an
//! [`OutcomeVerdict`]. L0 (tool errored / empty) is folded into the
//! [`OutcomePredicate::NoError`] / [`OutcomePredicate::NonEmpty`] checks; L1 is
//! the rest of the predicate matrix. The verdict's
//! [`needs_l2`](OutcomeVerdict::needs_l2) flag tells the caller when to escalate
//! to the single fast-model yes/no (L2) — this crate never makes that call.

use serde_json::Value;

use crate::model::{ExpectedOutcome, OutcomePredicate, OutcomeVerdict};

/// Planner confidence at or below this biases toward an L2 check even when
/// every L1 predicate passes (the "plausible but wrong" case, scope §1.2).
pub const L2_CONFIDENCE_THRESHOLD: f32 = 0.75;

/// Evaluate a realised tool `result` against its step's [`ExpectedOutcome`].
///
/// Pure and total: no I/O, no model calls, no panics. Every predicate is
/// checked (no short-circuit) so [`OutcomeVerdict::failed_predicates`] lists all
/// mismatches for the replan prompt and the bubbles.
///
/// Verdict semantics:
/// * `surprised` ⇔ at least one predicate failed (a definitive L1 mismatch).
/// * `needs_l2` ⇔ **not** surprised, there is a non-empty assertion to ask,
///   **and** L1 was inconclusive — either no predicates were declared
///   (assertion-only step) or they all passed but planner `confidence` is at or
///   below [`L2_CONFIDENCE_THRESHOLD`].
pub fn evaluate(expected: &ExpectedOutcome, result: &Value) -> OutcomeVerdict {
    let failed_predicates: Vec<OutcomePredicate> = expected
        .predicates
        .iter()
        .filter(|p| !predicate_holds(p, result))
        .cloned()
        .collect();

    let surprised = !failed_predicates.is_empty();

    let has_assertion = !expected.assertion.trim().is_empty();
    let l1_inconclusive =
        expected.predicates.is_empty() || expected.confidence <= L2_CONFIDENCE_THRESHOLD;
    let needs_l2 = !surprised && has_assertion && l1_inconclusive;

    OutcomeVerdict {
        surprised,
        failed_predicates,
        needs_l2,
    }
}

/// Does a single predicate hold against the result? Pure; never panics.
fn predicate_holds(predicate: &OutcomePredicate, result: &Value) -> bool {
    match predicate {
        OutcomePredicate::NonEmpty => !is_empty_value(result),
        OutcomePredicate::JsonPathExists { path } => result.pointer(path).is_some(),
        OutcomePredicate::JsonPathEquals { path, value } => result.pointer(path) == Some(value),
        OutcomePredicate::Contains { needle, ci } => {
            let haystack = serialise(result);
            if *ci {
                haystack.to_lowercase().contains(&needle.to_lowercase())
            } else {
                haystack.contains(needle.as_str())
            }
        }
        OutcomePredicate::CountAtLeast { path, n } => result
            .pointer(path)
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.len() >= *n),
        OutcomePredicate::NoError => !is_error_envelope(result),
    }
}

/// A value is "empty" when it is `null`, an empty string, an empty array, or an
/// empty object. Numbers and booleans (including `0` / `false`) are **not**
/// empty — they are values.
pub fn is_empty_value(result: &Value) -> bool {
    match result {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Structural heuristic for a tool-error envelope (L0, belt-and-braces). At P0
/// this is pure-shape detection; the real wiring (P5) reconciles it with the
/// live tool-result envelope + `ToolEvent::ToolError`. An object is an error
/// envelope when any of:
/// * it carries a non-null, non-empty `"error"` field, or
/// * `"ok"` is explicitly `false`, or
/// * `"status"` equals `"error"` (case-insensitive).
pub fn is_error_envelope(result: &Value) -> bool {
    let Some(obj) = result.as_object() else {
        return false;
    };
    if let Some(err) = obj.get("error") {
        if !is_empty_value(err) {
            return true;
        }
    }
    if obj.get("ok") == Some(&Value::Bool(false)) {
        return true;
    }
    if let Some(Value::String(status)) = obj.get("status") {
        if status.eq_ignore_ascii_case("error") {
            return true;
        }
    }
    false
}

/// Compact JSON serialisation for `Contains`. Strings serialise without their
/// surrounding quotes so a needle matches the inner text naturally; everything
/// else uses canonical compact JSON.
fn serialise(result: &Value) -> String {
    match result {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
