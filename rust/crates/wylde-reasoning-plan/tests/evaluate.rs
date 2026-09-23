//! P0 unit suite for `wylde-reasoning-plan`: the predicate matrix, the verdict
//! (L1 surprise + L2-escalation) logic, the inert-identity invariant, and a
//! serde round-trip of the plan model.

use serde_json::json;
use wylde_reasoning_plan::{
    evaluate, is_empty_value, is_error_envelope, ExpectedOutcome, OutcomePredicate, PlanDag,
    PlanStep, SurpriseAction, L2_CONFIDENCE_THRESHOLD,
};

/// Build an expectation from just a predicate list, full confidence, no
/// assertion — isolates L1 behaviour from the L2-escalation logic.
fn with_predicates(predicates: Vec<OutcomePredicate>) -> ExpectedOutcome {
    ExpectedOutcome {
        predicates,
        assertion: String::new(),
        on_surprise: SurpriseAction::Replan,
        confidence: 1.0,
    }
}

/// Convenience: does this single predicate hold against `result`? (Routed
/// through the public `evaluate` so we test the real surface.)
fn holds(predicate: OutcomePredicate, result: &serde_json::Value) -> bool {
    !evaluate(&with_predicates(vec![predicate]), result).surprised
}

// ---------------------------------------------------------------------------
// Predicate matrix
// ---------------------------------------------------------------------------

#[test]
fn non_empty_matrix() {
    // Empty values fail NonEmpty.
    for empty in [json!(null), json!(""), json!([]), json!({})] {
        assert!(
            !holds(OutcomePredicate::NonEmpty, &empty),
            "expected empty: {empty}"
        );
    }
    // Values (incl. 0 / false) pass NonEmpty.
    for value in [
        json!(0),
        json!(false),
        json!("x"),
        json!([1]),
        json!({"a":1}),
    ] {
        assert!(
            holds(OutcomePredicate::NonEmpty, &value),
            "expected non-empty: {value}"
        );
    }
}

#[test]
fn is_empty_value_helper() {
    assert!(is_empty_value(&json!(null)));
    assert!(is_empty_value(&json!("")));
    assert!(is_empty_value(&json!([])));
    assert!(is_empty_value(&json!({})));
    assert!(!is_empty_value(&json!(0)));
    assert!(!is_empty_value(&json!(false)));
}

#[test]
fn json_path_exists_matrix() {
    let result = json!({ "entries": [ { "name": "Cargo.toml" } ] });
    assert!(holds(
        OutcomePredicate::JsonPathExists {
            path: "/entries/0/name".into()
        },
        &result
    ));
    assert!(!holds(
        OutcomePredicate::JsonPathExists {
            path: "/entries/1/name".into()
        },
        &result
    ));
    assert!(!holds(
        OutcomePredicate::JsonPathExists {
            path: "/missing".into()
        },
        &result
    ));
    // Empty pointer = whole document, which exists.
    assert!(holds(
        OutcomePredicate::JsonPathExists { path: "".into() },
        &result
    ));
}

#[test]
fn json_path_equals_matrix() {
    let result = json!({ "ok": true, "count": 3 });
    assert!(holds(
        OutcomePredicate::JsonPathEquals {
            path: "/count".into(),
            value: json!(3)
        },
        &result
    ));
    assert!(!holds(
        OutcomePredicate::JsonPathEquals {
            path: "/count".into(),
            value: json!(4)
        },
        &result
    ));
    // Missing path never equals.
    assert!(!holds(
        OutcomePredicate::JsonPathEquals {
            path: "/nope".into(),
            value: json!(null)
        },
        &result
    ));
}

#[test]
fn contains_matrix() {
    let result = json!({ "path": "/repo/Cargo.toml" });
    // Case-sensitive hit + miss.
    assert!(holds(
        OutcomePredicate::Contains {
            needle: "Cargo.toml".into(),
            ci: false
        },
        &result
    ));
    assert!(!holds(
        OutcomePredicate::Contains {
            needle: "cargo.toml".into(),
            ci: false
        },
        &result
    ));
    // Case-insensitive recovers the miss.
    assert!(holds(
        OutcomePredicate::Contains {
            needle: "cargo.toml".into(),
            ci: true
        },
        &result
    ));
    // A bare string result matches its inner text (no surrounding quotes).
    assert!(holds(
        OutcomePredicate::Contains {
            needle: "hello".into(),
            ci: false
        },
        &json!("a hello world")
    ));
}

#[test]
fn count_at_least_matrix() {
    let result = json!({ "items": [1, 2, 3] });
    assert!(holds(
        OutcomePredicate::CountAtLeast {
            path: "/items".into(),
            n: 3
        },
        &result
    ));
    assert!(!holds(
        OutcomePredicate::CountAtLeast {
            path: "/items".into(),
            n: 4
        },
        &result
    ));
    // n == 0 always holds for a present array.
    assert!(holds(
        OutcomePredicate::CountAtLeast {
            path: "/items".into(),
            n: 0
        },
        &result
    ));
    // Non-array / absent path fails.
    assert!(!holds(
        OutcomePredicate::CountAtLeast {
            path: "/items/0".into(),
            n: 1
        },
        &result
    ));
    assert!(!holds(
        OutcomePredicate::CountAtLeast {
            path: "/missing".into(),
            n: 1
        },
        &result
    ));
}

#[test]
fn no_error_matrix() {
    // Error-envelope shapes.
    assert!(is_error_envelope(&json!({ "error": "boom" })));
    assert!(is_error_envelope(&json!({ "ok": false })));
    assert!(is_error_envelope(&json!({ "status": "error" })));
    assert!(is_error_envelope(&json!({ "status": "ERROR" }))); // ci
                                                               // Not errors.
    assert!(!is_error_envelope(&json!({ "ok": true })));
    assert!(!is_error_envelope(&json!({ "error": null }))); // empty error field
    assert!(!is_error_envelope(&json!({ "error": "" })));
    assert!(!is_error_envelope(&json!({ "status": "done" })));
    assert!(!is_error_envelope(&json!([1, 2, 3]))); // non-object
    assert!(!is_error_envelope(&json!("plain string")));

    // Through the predicate: NoError holds iff not an error envelope.
    assert!(holds(OutcomePredicate::NoError, &json!({ "ok": true })));
    assert!(!holds(OutcomePredicate::NoError, &json!({ "error": "x" })));
}

// ---------------------------------------------------------------------------
// Verdict logic (L1 surprise + L2 escalation)
// ---------------------------------------------------------------------------

#[test]
fn clean_pass_is_not_surprising_and_needs_no_l2() {
    let expected = with_predicates(vec![OutcomePredicate::NonEmpty, OutcomePredicate::NoError]);
    let verdict = evaluate(&expected, &json!({ "items": [1] }));
    assert!(!verdict.surprised);
    assert!(verdict.failed_predicates.is_empty());
    assert!(!verdict.needs_l2);
}

#[test]
fn failed_predicate_surprises_and_is_reported() {
    let expected = with_predicates(vec![
        OutcomePredicate::NonEmpty,
        OutcomePredicate::CountAtLeast {
            path: "/items".into(),
            n: 2,
        },
    ]);
    let verdict = evaluate(&expected, &json!({ "items": [1] }));
    assert!(verdict.surprised);
    assert_eq!(verdict.failed_predicates.len(), 1);
    assert_eq!(
        verdict.failed_predicates[0],
        OutcomePredicate::CountAtLeast {
            path: "/items".into(),
            n: 2
        }
    );
    // A definitive L1 surprise never escalates to L2.
    assert!(!verdict.needs_l2);
}

#[test]
fn all_failures_are_collected_not_short_circuited() {
    let expected = with_predicates(vec![
        OutcomePredicate::NonEmpty,
        OutcomePredicate::NoError,
        OutcomePredicate::JsonPathExists { path: "/x".into() },
    ]);
    // Empty object: NonEmpty fails, NoError holds (no error markers), path missing.
    let verdict = evaluate(&expected, &json!({}));
    assert!(verdict.surprised);
    assert_eq!(verdict.failed_predicates.len(), 2);
}

#[test]
fn assertion_only_step_needs_l2() {
    let expected = ExpectedOutcome {
        predicates: vec![],
        assertion: "the file list should contain a Cargo.toml at the repo root".into(),
        on_surprise: SurpriseAction::Replan,
        confidence: 1.0,
    };
    let verdict = evaluate(&expected, &json!({ "files": ["Cargo.toml"] }));
    assert!(!verdict.surprised);
    assert!(verdict.needs_l2);
}

#[test]
fn no_predicates_no_assertion_needs_no_l2() {
    let expected = ExpectedOutcome {
        predicates: vec![],
        assertion: "   ".into(), // whitespace-only counts as no assertion
        on_surprise: SurpriseAction::Continue,
        confidence: 1.0,
    };
    let verdict = evaluate(&expected, &json!({ "anything": true }));
    assert!(!verdict.surprised);
    assert!(!verdict.needs_l2);
}

#[test]
fn low_confidence_forces_l2_even_when_l1_passes() {
    let expected = ExpectedOutcome {
        predicates: vec![OutcomePredicate::NonEmpty],
        assertion: "is this the right repo root?".into(),
        on_surprise: SurpriseAction::Replan,
        confidence: 0.5, // below threshold
    };
    let verdict = evaluate(&expected, &json!({ "files": ["x"] }));
    assert!(!verdict.surprised);
    assert!(verdict.needs_l2, "low confidence should bias toward L2");
}

#[test]
fn high_confidence_clean_l1_skips_l2() {
    let expected = ExpectedOutcome {
        predicates: vec![OutcomePredicate::NonEmpty],
        assertion: "looks right?".into(),
        on_surprise: SurpriseAction::Replan,
        confidence: 0.95, // above threshold → L1 is conclusive
    };
    let verdict = evaluate(&expected, &json!({ "files": ["x"] }));
    assert!(!verdict.surprised);
    assert!(!verdict.needs_l2);
}

#[test]
fn confidence_threshold_is_inclusive() {
    // At exactly the threshold, L2 is requested (`<=`).
    let at = ExpectedOutcome {
        predicates: vec![OutcomePredicate::NonEmpty],
        assertion: "check".into(),
        on_surprise: SurpriseAction::Replan,
        confidence: L2_CONFIDENCE_THRESHOLD,
    };
    assert!(evaluate(&at, &json!([1])).needs_l2);

    // Just above, it is not.
    let above = ExpectedOutcome {
        confidence: L2_CONFIDENCE_THRESHOLD + 0.01,
        ..at.clone()
    };
    assert!(!evaluate(&above, &json!([1])).needs_l2);
}

// ---------------------------------------------------------------------------
// Inert identity invariant
// ---------------------------------------------------------------------------

#[test]
fn trusting_expectation_is_never_surprising() {
    // The data-level analogue of off ⇒ identity: a fully-trusted step never
    // surprises and never escalates, whatever the result.
    let expected = ExpectedOutcome::trusting();
    for result in [
        json!(null),
        json!({}),
        json!({ "error": "boom" }),
        json!([1, 2, 3]),
        json!("anything"),
    ] {
        let verdict = evaluate(&expected, &result);
        assert!(!verdict.surprised, "trusting must not surprise on {result}");
        assert!(!verdict.needs_l2, "trusting must not need L2 on {result}");
        assert!(verdict.failed_predicates.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Serde round-trip of the plan model
// ---------------------------------------------------------------------------

#[test]
fn plan_dag_round_trips_through_json() {
    let dag = PlanDag {
        goal: "find the workspace manifest".into(),
        reasoning_trace: "<think>scan the repo root</think>".into(),
        plan_version: 1,
        steps: vec![PlanStep {
            id: "s1".into(),
            intent: "list repo root".into(),
            tool: Some("fs.list".into()),
            args_template: json!({ "path": "." }),
            depends_on: vec![],
            expected: ExpectedOutcome {
                predicates: vec![
                    OutcomePredicate::NonEmpty,
                    OutcomePredicate::Contains {
                        needle: "Cargo.toml".into(),
                        ci: false,
                    },
                ],
                assertion: "the listing should include the workspace manifest".into(),
                on_surprise: SurpriseAction::Replan,
                confidence: 0.8,
            },
        }],
    };

    let text = serde_json::to_string(&dag).expect("serialise");
    let back: PlanDag = serde_json::from_str(&text).expect("deserialise");
    assert_eq!(dag, back);

    // The tagged predicate enum uses the documented `kind` discriminant.
    assert!(text.contains("\"kind\":\"non_empty\""));
    assert!(text.contains("\"kind\":\"contains\""));
    assert!(text.contains("\"on_surprise\":\"replan\""));
}
