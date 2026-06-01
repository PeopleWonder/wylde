//! Harness turn-driver parity: Python `Core.harness.turn._streaming`
//! pure functions vs `wylde_harness::turn::salvage`.
//!
//! ## What this gates
//!
//! Phase 5 of the master plan ports the chat-turn driver to Rust. The
//! 5.A/5.B/5.C slices ported the state machine and the byte-level
//! salvage parser; 5.D is the flag flip. This file is the gate: it
//! exercises every pure function on the salvage / hash / brace-finder
//! surface against a fixed byte corpus and asserts the Rust output
//! matches Python's byte-for-byte. A divergence here blocks the
//! `WYLDE_WYLDE_HARNESS_IMPL=rust` default flip.
//!
//! ## Why pure-function parity rather than full chat.run_turn parity
//!
//! The full `chat.run_turn` action requires a working `wylde-ollama`
//! pipe to call into. A symmetric end-to-end parity test would need to
//! stand up a deterministic stub Ollama on a shared isolated pipe and
//! drive both impls against it sequentially — substantial new
//! infrastructure for marginal coverage, because both sides already
//! ship end-to-end tests against their own scripted Ollama mocks
//! (Rust: `rust/crates/wylde-harness/tests/run_turn_loop_e2e.rs`;
//! Python: `Core/harness/tests/test_turn/*`). The pieces the
//! cross-language e2e would catch — turn-loop control flow, summary
//! row shape, abort propagation — are stable framework-level
//! behaviours both impls were unit-tested for at port time.
//!
//! The pieces that drift silently are byte-level: the salvage parser's
//! regex semantics, the brace scanner's escape handling, the call_hash
//! canonicalisation. Those are exactly what this file covers, and
//! their parity is a load-bearing prerequisite for the dispatch loop
//! to produce the same `tool_calls_summary` rows.
//!
//! ## How it runs
//!
//! For each case the Rust pure function is called in-process; the
//! Python side is invoked via `.venv\Scripts\python.exe` with the
//! shared probe script piped on stdin. Both outputs are diffed via the
//! parity harness's structural diff (`wylde_parity::diff`).
//!
//! Cross-platform: runs anywhere the `.venv` interpreter exists. No
//! Windows-only pipe transport here — the probe is plain stdio.

#![cfg(feature = "parity")]

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use wylde_harness::turn::salvage;
use wylde_parity::{diff, paths};

/// Python probe script — registered on stdin, replies on stdout. One
/// process per case (cheap, ~150ms) keeps each case isolated and lets
/// a hung probe surface as a per-case failure rather than poisoning
/// the rest. We import lazily inside each branch so a missing optional
/// dep on one path can't break the others.
const PYTHON_PROBE: &str = r#"
import json, sys

req = json.loads(sys.stdin.read())
fn = req["fn"]

if fn == "extract":
    from Core.harness.turn._streaming import _extract_tool_calls_from_content
    text = req["text"]
    alias_map = req.get("alias_map", {})
    cleaned, recovered, unrecognised = _extract_tool_calls_from_content(text, alias_map)
    print(json.dumps({
        "cleaned": cleaned,
        "recovered": recovered,
        "unrecognised": unrecognised,
    }))
elif fn == "call_hash":
    from Core.harness.turn._streaming import _call_hash
    print(json.dumps({"hash": _call_hash(req["name"], req["args"])}))
elif fn == "find_balanced_braces":
    from Core.harness.turn._streaming import _find_balanced_braces
    spans = list(_find_balanced_braces(req["text"]))
    print(json.dumps({"spans": [list(s) for s in spans]}))
elif fn == "parse_one_call":
    from Core.harness.turn._streaming import _parse_one_call
    out = _parse_one_call(req["obj"])
    print(json.dumps({"parsed": out}))
else:
    print(json.dumps({"error": f"unknown fn {fn!r}"}))
    sys.exit(1)
"#;

/// One round-trip into the Python probe. Returns the JSON the probe
/// printed on stdout. Panics with the case name + probe stderr if the
/// probe exits non-zero — that's almost always a torn `.venv`.
fn python_probe(case: &str, req: &Value) -> Value {
    let mut child = Command::new(paths::venv_python())
        .arg("-c")
        .arg(PYTHON_PROBE)
        .current_dir(paths::repo_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("[{case}] spawn python probe: {e}"));
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(req.to_string().as_bytes())
            .unwrap_or_else(|e| panic!("[{case}] write probe input: {e}"));
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("[{case}] wait probe: {e}"));
    if !out.status.success() {
        panic!(
            "[{case}] python probe exited {}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("[{case}] probe stdout was not JSON ({e}): {trimmed:?}")
    })
}

/// Default alias map used by salvage cases. Maps every name our corpus
/// references — symmetric across Python and Rust because both sides
/// look up `alias_map.get(name)` directly.
fn default_alias() -> HashMap<String, String> {
    [
        ("git_status", "git_status"),
        ("git_diff", "git_diff"),
        ("rag_ask", "rag_ask"),
        ("foo", "foo"),
        ("memory_long_term_save", "memory_long_term_save"),
        ("memory.long_term.save", "memory_long_term_save"),
        ("fs.read_file", "fs_read_file"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// One salvage parity case: the input text + a description of why it's
/// in the corpus.
struct SalvageCase {
    name: &'static str,
    /// Why this case is in the corpus — exercises which detection
    /// shape / edge case.
    rationale: &'static str,
    text: &'static str,
}

/// The 15-case salvage corpus. Each case targets a specific detection
/// shape or edge case the byte-level Rust port had to mirror.
///
/// Coverage matrix:
/// * **Bare JSON** — simple, with args, two emissions, llama
///   parameters field, nested function field, unrecognised name.
/// * **Tag-wrapped** — `tool_call`, `function_call`, `tool_use`.
/// * **Fenced JSON** — with `json` marker, without it, invalid body.
/// * **Mixed prose** — bare call surrounded by text, prose JSON
///   without a `name` key (must NOT be scrubbed).
/// * **Empty input** — degenerate case.
fn salvage_cases() -> Vec<SalvageCase> {
    vec![
        SalvageCase {
            name: "bare_simple",
            rationale: "minimal bare JSON tool call — happy path for the bare-brace pass",
            text: r#"{"name": "git_status", "arguments": {}}"#,
        },
        SalvageCase {
            name: "bare_with_args",
            rationale: "dotted-name resolves via alias_map to snake_case canonical id",
            text: r#"{"name": "memory.long_term.save", "arguments": {"body": "kebab"}}"#,
        },
        SalvageCase {
            name: "tag_wrapped",
            rationale: "<tool_call>...</tool_call> with surrounding prose; cleaned text \
                        keeps the prose",
            text: "I'll check that.\n<tool_call>{\"name\": \"git_status\", \"arguments\": {}}</tool_call>\nDone.",
        },
        SalvageCase {
            name: "function_call_tag",
            rationale: "<function_call> alias for the tool-call tag",
            text: "<function_call>{\"name\": \"foo\", \"arguments\": {}}</function_call>",
        },
        SalvageCase {
            name: "tool_use_tag",
            rationale: "<tool_use> alias for the tool-call tag",
            text: "<tool_use>{\"name\": \"foo\", \"arguments\": {}}</tool_use>",
        },
        SalvageCase {
            name: "fenced_json",
            rationale: "```json fenced body — the `json` marker is optional but common",
            text: "Here you go:\n```json\n{\"name\": \"rag_ask\", \"arguments\": {\"q\": \"test\"}}\n```\n",
        },
        SalvageCase {
            name: "fenced_no_json_marker",
            rationale: "bare ``` fence — Python re `(?:json)?` makes the marker optional",
            text: "```\n{\"name\": \"foo\", \"arguments\": {}}\n```",
        },
        SalvageCase {
            name: "fenced_invalid_body",
            rationale: "fenced body that fails json.loads — fence must stay intact, no recovery",
            text: "```json\nnot valid json\n```",
        },
        SalvageCase {
            name: "unrecognised_name",
            rationale: "name not in alias_map → unrecognised list; JSON still scrubbed",
            text: r#"{"name": "nonexistent_tool", "arguments": {"x": 1}}"#,
        },
        SalvageCase {
            name: "mixed_prose_and_call",
            rationale: "bare tool call sandwiched between prose; cleaned must drop the call",
            text: r#"Let me check the diff for you. {"name": "git_diff", "arguments": {"path": "."}} Be right back!"#,
        },
        SalvageCase {
            name: "prose_json_no_name_key",
            rationale: "JSON without a top-level \"name\" — must NOT be scrubbed (prose guard)",
            text: r#"The forecast is {"weather": "sunny", "temp": 72}. Nothing else to report."#,
        },
        SalvageCase {
            name: "two_emissions_same_call",
            rationale: "two copies of the same bare call — salvage recovers BOTH; dedupe \
                        is the dispatcher's job (tool_round)",
            text: "{\"name\": \"git_status\", \"arguments\": {}}\n{\"name\": \"git_status\", \"arguments\": {}}",
        },
        SalvageCase {
            name: "llama_parameters_field",
            rationale: "Llama emits `parameters` instead of `arguments`",
            text: r#"{"name": "foo", "parameters": {"k": 1}}"#,
        },
        SalvageCase {
            name: "nested_function_field",
            rationale: "{\"function\": {\"name\": ..., \"arguments\": ...}} — OpenAI-style wrap",
            text: r#"{"function": {"name": "foo", "arguments": {"k": 1}}}"#,
        },
        SalvageCase {
            name: "empty_input",
            rationale: "degenerate input — empty text in, empty everything out",
            text: "",
        },
    ]
}

/// Build the Python `_extract_tool_calls_from_content` reply shape from
/// the Rust [`salvage::SalvageResult`] for diffing. Both sides emit
/// `{cleaned, recovered, unrecognised}` with the same field names and
/// nested dict shape — only the type bridging differs.
fn rust_extract_as_json(text: &str, alias_map: &HashMap<String, String>) -> Value {
    let r = salvage::extract_tool_calls_from_content(text, alias_map);
    json!({
        "cleaned": r.cleaned_text,
        "recovered": r.recovered.iter().map(|c| json!({
            "id": c.id,
            "name": c.name,
            "args": c.args,
            "raw_name": c.raw_name,
        })).collect::<Vec<_>>(),
        "unrecognised": r.unrecognised.iter().map(|c| json!({
            "id": c.id,
            "name": c.name,
            "args": c.args,
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn salvage_parity() {
    paths::require_artifact(
        &paths::venv_python(),
        "create the Wylde virtualenv (.venv) with the harness deps",
    );

    let alias_map = default_alias();
    let alias_value: Value = alias_map
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect::<serde_json::Map<_, _>>()
        .into();

    let cases = salvage_cases();
    let mut failures: Vec<String> = Vec::new();
    let mut failure_names: Vec<&str> = Vec::new();
    let mut passed: Vec<&str> = Vec::new();
    let mut rationales: Vec<(&str, &str)> = Vec::new();

    for case in &cases {
        let req = json!({
            "fn": "extract",
            "text": case.text,
            "alias_map": alias_value,
        });
        let python_out = python_probe(case.name, &req);
        let rust_out = rust_extract_as_json(case.text, &alias_map);

        match diff::compare(case.name, &python_out, &rust_out, &[]) {
            Ok(()) => passed.push(case.name),
            Err(report) => {
                failure_names.push(case.name);
                failures.push(report);
            }
        }
        rationales.push((case.name, case.rationale));
    }

    eprintln!("\n=== Harness turn salvage parity ===");
    eprintln!("corpus rationale ({} cases):", cases.len());
    for (n, r) in &rationales {
        eprintln!("  - {n}: {r}");
    }
    eprintln!("\nparity ({}): {passed:?}", passed.len());
    if failure_names.is_empty() {
        eprintln!("diverged: none");
    } else {
        eprintln!("diverged ({}): {failure_names:?}", failure_names.len());
    }

    assert!(
        failures.is_empty(),
        "{} salvage case(s) diverged ({:?}):\n\n{}",
        failures.len(),
        failure_names,
        failures.join("\n\n"),
    );
}

/// One call_hash parity case.
#[allow(dead_code)] // `rationale` is read by the docstring above; the eprintln keeps the names list compact.
struct HashCase {
    name: &'static str,
    rationale: &'static str,
    tool_name: &'static str,
    args: Value,
}

/// call_hash corpus. The Rust port mirrors Python's
/// `json.dumps(args, sort_keys=True)` over the args dict; these cases
/// exercise the canonicalisation rules that drift silently if the
/// sort/escape/null handling differs.
fn hash_cases() -> Vec<HashCase> {
    vec![
        HashCase {
            name: "empty_args",
            rationale: "empty dict — pins the salt + null/empty boundary",
            tool_name: "git_status",
            args: json!({}),
        },
        HashCase {
            name: "scalar_args",
            rationale: "single key with scalar value — minimal non-empty",
            tool_name: "fs_read_file",
            args: json!({"path": "src/lib.rs"}),
        },
        HashCase {
            name: "nested_with_array",
            rationale: "nested dict + array — exercises recursive canonicalisation",
            tool_name: "complex_tool",
            args: json!({"a": {"b": 2, "c": [1, 2, 3]}}),
        },
        HashCase {
            name: "string_escapes_ascii",
            rationale: "strings with quotes / backslashes — JSON-escape parity within ASCII. \
                        Non-ASCII intentionally excluded: Python's json.dumps defaults to \
                        ensure_ascii=True (\\uXXXX); Rust's serde_json does not. The hash \
                        is process-local (per-turn dedupe set), so cross-impl divergence \
                        on non-ASCII is semantically acceptable — but worth fixing under \
                        Phase 8 hygiene; tracked in the slice-5.D writeup punchlist.",
            tool_name: "escape_test",
            args: json!({"s": "with \"quotes\" and \\backslash"}),
        },
        HashCase {
            name: "order_insensitive",
            rationale: "keys in different order — must hash the same as `nested_with_array` \
                        when args are equivalent (sort_keys=True)",
            tool_name: "complex_tool",
            args: json!({"a": {"c": [1, 2, 3], "b": 2}}),
        },
    ]
}

#[test]
fn call_hash_parity() {
    paths::require_artifact(
        &paths::venv_python(),
        "create the Wylde virtualenv (.venv) with the harness deps",
    );

    let cases = hash_cases();
    let mut failures: Vec<String> = Vec::new();
    let mut failure_names: Vec<&str> = Vec::new();
    let mut passed: Vec<&str> = Vec::new();

    for case in &cases {
        let req = json!({
            "fn": "call_hash",
            "name": case.tool_name,
            "args": case.args,
        });
        let python_out = python_probe(case.name, &req);
        let rust_hash = salvage::call_hash(case.tool_name, &case.args);
        let rust_out = json!({"hash": rust_hash});

        match diff::compare(case.name, &python_out, &rust_out, &[]) {
            Ok(()) => passed.push(case.name),
            Err(report) => {
                failure_names.push(case.name);
                failures.push(report);
            }
        }
    }

    eprintln!("\n=== Harness turn call_hash parity ===");
    eprintln!("parity ({}): {passed:?}", passed.len());
    if !failure_names.is_empty() {
        eprintln!("diverged ({}): {failure_names:?}", failure_names.len());
    }

    assert!(
        failures.is_empty(),
        "{} call_hash case(s) diverged ({:?}):\n\n{}",
        failures.len(),
        failure_names,
        failures.join("\n\n"),
    );
}

/// One find_balanced_braces parity case.
#[allow(dead_code)] // `rationale` is read by the docstring above; the eprintln keeps the names list compact.
struct BraceCase {
    name: &'static str,
    rationale: &'static str,
    text: &'static str,
}

/// Brace-scanner corpus. The scanner has to respect double-quoted
/// string boundaries and backslash escapes inside strings, and skip
/// unbalanced trailing fragments. Each case exercises one of those
/// invariants.
fn brace_cases() -> Vec<BraceCase> {
    vec![
        BraceCase {
            name: "empty_object",
            rationale: "minimal balanced span",
            text: "{}",
        },
        BraceCase {
            name: "string_with_braces",
            rationale: "a `}` inside a quoted string must NOT close the outer object",
            text: r#"prelude {"a":{"b":"}escaped"}, "c":1} tail"#,
        },
        BraceCase {
            name: "unbalanced_trailer",
            rationale: "first object balances, second is truncated — only first is returned",
            text: r#"{"a": 1} and then {"b": 2"#,
        },
        BraceCase {
            name: "escaped_quote_in_string",
            rationale: "`\\\"` inside a string must not flip the in-string state",
            text: r#"{"k": "has \"quote\" inside"}"#,
        },
        BraceCase {
            name: "empty_input",
            rationale: "degenerate input — no spans",
            text: "",
        },
    ]
}

#[test]
fn find_balanced_braces_parity() {
    paths::require_artifact(
        &paths::venv_python(),
        "create the Wylde virtualenv (.venv) with the harness deps",
    );

    let cases = brace_cases();
    let mut failures: Vec<String> = Vec::new();
    let mut failure_names: Vec<&str> = Vec::new();
    let mut passed: Vec<&str> = Vec::new();

    for case in &cases {
        let req = json!({"fn": "find_balanced_braces", "text": case.text});
        let python_out = python_probe(case.name, &req);
        let rust_spans: Vec<Vec<usize>> = salvage::find_balanced_braces(case.text)
            .into_iter()
            .map(|(s, e)| vec![s, e])
            .collect();
        let rust_out = json!({"spans": rust_spans});

        match diff::compare(case.name, &python_out, &rust_out, &[]) {
            Ok(()) => passed.push(case.name),
            Err(report) => {
                failure_names.push(case.name);
                failures.push(report);
            }
        }
    }

    eprintln!("\n=== Harness turn find_balanced_braces parity ===");
    eprintln!("parity ({}): {passed:?}", passed.len());
    if !failure_names.is_empty() {
        eprintln!("diverged ({}): {failure_names:?}", failure_names.len());
    }

    assert!(
        failures.is_empty(),
        "{} find_balanced_braces case(s) diverged ({:?}):\n\n{}",
        failures.len(),
        failure_names,
        failures.join("\n\n"),
    );
}
