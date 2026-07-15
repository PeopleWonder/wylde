//! The canonical JSON Schema for [`crate::PlanDag`] — the value handed to
//! Ollama's `format` parameter for grammar-constrained PLAN decoding.
//!
//! ## Why this exists
//!
//! The 2026-07-13 reasoner eval (15 grounded planning prompts, validity =
//! deserializing into the real [`crate::PlanDag`] serde types) showed that
//! grammar-constrained decoding eliminates exactly the failure class that
//! survives good models: the default reasoner (`qwen3.6:35b-a3b`
//! UD-IQ3_XXS) went 93.3% → **100%** valid, at unchanged speed and with no
//! plan-quality loss, when the request carried this schema as `format`.
//! (It does NOT rescue weak models: `qwen3.5:9b` went 46.7% → only 73.3% —
//! the grammar constrains the *content* segment, not `<think>`, so
//! think-loop rumination still exhausts the token budget.)
//!
//! ## Contract with the serde types
//!
//! This schema MUST stay field-for-field in lockstep with
//! [`crate::model`]'s serde shape — every `required` list mirrors the
//! struct's non-defaulted fields, enum strings mirror the
//! `rename_all = "snake_case"` wire form, and [`crate::OutcomePredicate`]'s
//! internally-tagged (`tag = "kind"`) union is spelled as an `anyOf` of six
//! closed objects. A drift means the grammar can force output serde then
//! rejects (or vice versa); the tests below pin the coupling in both
//! directions.
//!
//! ## Backend behaviour notes (verified live 2026-07-13, Ollama on the dev
//! rig)
//!
//! * `format` constrains only `message.content`; `message.thinking` streams
//!   unconstrained (byte-identical think text vs the unconstrained run at
//!   the same seed). No think/format conflict — do NOT disable thinking on
//!   PLAN calls.
//! * This Ollama build never rejects a malformed schema — it silently
//!   proceeds unconstrained (HTTP 200). The fail-soft retry in the harness
//!   (`turn/reasoning/constrained.rs`) exists for backends/versions that DO
//!   reject.

use serde_json::{json, Value};

/// JSON Schema for one [`crate::OutcomePredicate`] — an `anyOf` over the
/// six `kind`-tagged variants, each a closed object requiring exactly the
/// fields serde requires.
fn predicate_schema() -> Value {
    json!({
        "anyOf": [
            {"type": "object", "properties": {"kind": {"const": "non_empty"}},
             "required": ["kind"], "additionalProperties": false},
            {"type": "object", "properties": {"kind": {"const": "json_path_exists"},
             "path": {"type": "string"}},
             "required": ["kind", "path"], "additionalProperties": false},
            {"type": "object", "properties": {"kind": {"const": "json_path_equals"},
             "path": {"type": "string"}, "value": {}},
             "required": ["kind", "path", "value"], "additionalProperties": false},
            {"type": "object", "properties": {"kind": {"const": "contains"},
             "needle": {"type": "string"}, "ci": {"type": "boolean"}},
             "required": ["kind", "needle", "ci"], "additionalProperties": false},
            {"type": "object", "properties": {"kind": {"const": "count_at_least"},
             "path": {"type": "string"}, "n": {"type": "integer", "minimum": 0}},
             "required": ["kind", "path", "n"], "additionalProperties": false},
            {"type": "object", "properties": {"kind": {"const": "no_error"}},
             "required": ["kind"], "additionalProperties": false}
        ]
    })
}

/// The full [`crate::PlanDag`] schema, ready to pass as Ollama's `format`.
///
/// Built fresh per call (it's a handful of allocations on a per-deep-turn
/// path); callers that care can cache the `Value`.
pub fn plan_dag_format() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal": {"type": "string"},
            "steps": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "intent": {"type": "string"},
                        "tool": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                        "args_template": {"type": "object"},
                        "depends_on": {"type": "array", "items": {"type": "string"}},
                        "expected": {
                            "type": "object",
                            "properties": {
                                "predicates": {"type": "array", "items": predicate_schema()},
                                "assertion": {"type": "string"},
                                "on_surprise": {"enum": ["replan", "continue", "abort"]},
                                "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                            },
                            "required": ["predicates", "assertion", "on_surprise", "confidence"],
                            "additionalProperties": false
                        }
                    },
                    "required": ["id", "intent", "tool", "args_template", "depends_on", "expected"],
                    "additionalProperties": false
                }
            },
            "reasoning_trace": {"type": "string"},
            "plan_version": {"type": "integer", "minimum": 1}
        },
        "required": ["goal", "steps", "reasoning_trace", "plan_version"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExpectedOutcome, OutcomePredicate, PlanDag, PlanStep, SurpriseAction};

    /// A sample touching every variant the schema must admit.
    fn sample_dag() -> PlanDag {
        PlanDag {
            goal: "g".into(),
            steps: vec![PlanStep {
                id: "s1".into(),
                intent: "i".into(),
                tool: Some("workspaces.rag_query".into()),
                args_template: serde_json::json!({"query": "${s0.output}"}),
                depends_on: vec![],
                expected: ExpectedOutcome {
                    predicates: vec![
                        OutcomePredicate::NonEmpty,
                        OutcomePredicate::JsonPathExists { path: "/m".into() },
                        OutcomePredicate::JsonPathEquals {
                            path: "/ok".into(),
                            value: serde_json::json!(true),
                        },
                        OutcomePredicate::Contains {
                            needle: "x".into(),
                            ci: true,
                        },
                        OutcomePredicate::CountAtLeast {
                            path: "/m".into(),
                            n: 1,
                        },
                        OutcomePredicate::NoError,
                    ],
                    assertion: String::new(),
                    on_surprise: SurpriseAction::Replan,
                    confidence: 0.8,
                },
            }],
            reasoning_trace: "r".into(),
            plan_version: 1,
        }
    }

    /// Direction 1 (schema → serde): every key combination the schema's
    /// `required`+`additionalProperties:false` admits must deserialize.
    /// Spot-pinned by the minimal admissible object per level.
    #[test]
    fn minimal_schema_conformant_object_deserializes() {
        let minimal = serde_json::json!({
            "goal": "g",
            "steps": [{
                "id": "s1", "intent": "i", "tool": null, "args_template": {},
                "depends_on": [],
                "expected": {
                    "predicates": [], "assertion": "",
                    "on_surprise": "continue", "confidence": 1.0
                }
            }],
            "reasoning_trace": "",
            "plan_version": 1
        });
        serde_json::from_value::<PlanDag>(minimal).expect("schema-minimal object must deserialize");
    }

    /// Direction 2 (serde → schema): a serialized PlanDag must carry every
    /// key the schema requires with the spelled types — pinned structurally
    /// (no jsonschema validator dep; the harness's live eval covered full
    /// grammar conformance end-to-end).
    #[test]
    fn serialized_dag_carries_every_required_key() {
        let v = serde_json::to_value(sample_dag()).unwrap();
        let schema = plan_dag_format();

        let req: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        for k in req {
            assert!(v.get(k).is_some(), "dag missing top-level required key {k}");
        }
        let step_schema = &schema["properties"]["steps"]["items"];
        let step = &v["steps"][0];
        for k in step_schema["required"].as_array().unwrap() {
            assert!(step.get(k.as_str().unwrap()).is_some(), "step missing {k}");
        }
        let exp_schema = &step_schema["properties"]["expected"];
        for k in exp_schema["required"].as_array().unwrap() {
            assert!(
                step["expected"].get(k.as_str().unwrap()).is_some(),
                "expected missing {k}"
            );
        }
    }

    /// The predicate union must have exactly one arm per serde variant and
    /// spell each `kind` in the serde wire form.
    #[test]
    fn predicate_union_mirrors_serde_variants() {
        let arms = predicate_schema()["anyOf"].as_array().unwrap().clone();
        assert_eq!(arms.len(), 6, "one arm per OutcomePredicate variant");
        let kinds: Vec<String> = arms
            .iter()
            .map(|a| {
                a["properties"]["kind"]["const"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "non_empty",
                "json_path_exists",
                "json_path_equals",
                "contains",
                "count_at_least",
                "no_error"
            ]
        );
        // Every serde variant serializes to a kind the union admits.
        for p in sample_dag().steps[0].expected.predicates.iter() {
            let v = serde_json::to_value(p).unwrap();
            let k = v["kind"].as_str().unwrap();
            assert!(kinds.iter().any(|x| x == k), "unadmitted kind {k}");
        }
    }

    /// `on_surprise` enum strings mirror serde's snake_case wire form.
    #[test]
    fn on_surprise_enum_mirrors_wire_form() {
        let schema = plan_dag_format();
        let allowed = &schema["properties"]["steps"]["items"]["properties"]["expected"]
            ["properties"]["on_surprise"]["enum"];
        for (action, wire) in [
            (SurpriseAction::Replan, "replan"),
            (SurpriseAction::Continue, "continue"),
            (SurpriseAction::Abort, "abort"),
        ] {
            assert_eq!(
                serde_json::to_value(action).unwrap(),
                serde_json::json!(wire)
            );
            assert!(
                allowed.as_array().unwrap().iter().any(|v| v == wire),
                "{wire} missing from schema enum"
            );
        }
    }
}
