//! Per-call tool dispatch — tier gating, per-turn dedupe, runner
//! invocation, and `ToolEvent` emission. Rust port of
//! `Core/harness/turn/_tool_round.py`.
//!
//! Owns one tool call from the moment the turn loop hands it off to the
//! moment a [`ToolEvent::ToolResult`] or [`ToolEvent::ToolError`] event
//! lands on the per-turn buffer. Plus the per-call dedupe set that
//! suppresses repeated `(name, args)` invocations within a single turn.
//!
//! ## Slice 5.C scope
//!
//! * `MAX_TOOL_LOOPS` cap (8) on the surrounding tool-round loop.
//! * Per-turn dedupe via [`crate::turn::salvage::call_hash`].
//! * Tier gate over `device_tier` (`read_only`, `tool_use`,
//!   `destructive_tool_access`). Phase 6 wires the registry-driven
//!   "is this tool destructive" lookup; the gate exists today and
//!   defaults to allowing the call when the registry isn't reachable.
//! * Routing via [`crate::dispatch::route`].
//! * Memory-write event emission on canonical memory-tool ids.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::config::Config;
use crate::dispatch::{self, Route};
use crate::events::{
    MemoryWriteScope, MemoryWriteSource, ToolErrorReason, ToolEvent,
};
use crate::state::TurnHandle;
use crate::tooling::registry::Registry;
use crate::turn::salvage::{call_hash, RecoveredCall};

/// Hard ceiling on tool-call rounds per turn. Mirrors Python's
/// `_MAX_TOOL_LOOPS = 8` in `_driver.py:220`. Surfaced as a constant
/// rather than a config value because the Python side hard-codes it
/// and parity matters; `Config::max_tool_loops` is the per-process
/// override and defaults to the same.
pub const MAX_TOOL_LOOPS: usize = 8;

// Device-permission tiers — string constants. Mirror
// `Core/harness/turn/_tool_round.py:48-52`.
pub const TIER_READ_ONLY: &str = "read_only";
pub const TIER_TOOL_USE: &str = "tool_use";
pub const TIER_DESTRUCTIVE: &str = "destructive_tool_access";
pub const DEFAULT_TIER: &str = TIER_TOOL_USE;

/// Tool ids that perform a write against the memory layer. Matches
/// Python's `_MEMORY_WRITE_TOOL_IDS` in `_tool_round.py:34-40`. The
/// canonical (snake-case) form is what the salvage parser produces
/// after alias resolution.
pub const MEMORY_WRITE_TOOL_IDS: &[&str] = &[
    "memory_long_term_save",
    "memory_workspace_save",
    "memory_update",
];

/// Normalise an incoming device-tier string. Empty / unknown / `None`
/// fall back to `tool_use` — in-process callers (the GUI on the local
/// pipe, Voice) don't carry a bearer token and aren't expected to
/// thread one through; they're already inside the trust boundary.
pub fn normalise_device_tier(tier: Option<&str>) -> &'static str {
    match tier {
        Some(TIER_READ_ONLY) => TIER_READ_ONLY,
        Some(TIER_TOOL_USE) => TIER_TOOL_USE,
        Some(TIER_DESTRUCTIVE) => TIER_DESTRUCTIVE,
        _ => DEFAULT_TIER,
    }
}

/// Tier-gate decision. `None` → allowed. `Some(_)` → blocked with a
/// machine-readable reason + a user-visible error string.
struct GateBlock {
    reason: ToolErrorReason,
    error: String,
}

fn check_tier_gate(
    device_tier: &str,
    tool_name: &str,
    registry: &Registry,
) -> Option<GateBlock> {
    let tier = if device_tier.is_empty() {
        DEFAULT_TIER
    } else {
        device_tier
    };
    match tier {
        TIER_READ_ONLY => Some(GateBlock {
            reason: ToolErrorReason::TierReadOnly,
            error: format!(
                "tool {tool_name:?} blocked: device tier is 'read_only', \
                 no tools may run on this turn"
            ),
        }),
        TIER_DESTRUCTIVE => None,
        // tier == "tool_use" → block destructive internal tools. Phase 6
        // wires this: the registry knows each tool's `destructive` flag,
        // and the gate reads it directly. Unknown tools (e.g. extension
        // tools whose namespace is in `Config::mcp_namespaces`) bypass
        // the gate — extensions get vetted through their own MCP policy.
        _ => registry.lookup(tool_name).and_then(|entry| {
            if entry.destructive {
                Some(GateBlock {
                    reason: ToolErrorReason::TierReadOnly,
                    error: format!(
                        "tool {tool_name:?} blocked: device tier is \
                         'tool_use' but {:?} is destructive; needs \
                         'destructive_tool_access' tier",
                        entry.name
                    ),
                })
            } else {
                None
            }
        }),
    }
}

/// One tool call ready to dispatch — produced either by structured
/// `tool_calls` decode or by the salvage parser. Mirrors Python's
/// `ToolCall` dataclass (`id`, `name`, `args`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

impl From<RecoveredCall> for ToolCall {
    fn from(r: RecoveredCall) -> Self {
        Self {
            id: r.id,
            name: r.name,
            args: r.args,
        }
    }
}

/// One row of `state.tool_calls_summary` — mirrors Python's per-call
/// summary dict (`{call_id, name, ok, duration_ms[, error, reason]}`).
/// Kept as a `serde_json::Value` builder rather than a typed struct so
/// the wire shape matches the Python long-poll JSON envelope exactly.
#[derive(Debug, Clone)]
pub struct ToolSummary(pub Value);

impl ToolSummary {
    fn ok(call_id: &str, name: &str, duration_ms: u64) -> Self {
        Self(json!({
            "call_id": call_id,
            "name": name,
            "ok": true,
            "duration_ms": duration_ms,
        }))
    }

    fn err(
        call_id: &str,
        name: &str,
        duration_ms: u64,
        error: &str,
        reason: Option<ToolErrorReason>,
    ) -> Self {
        let mut obj = json!({
            "call_id": call_id,
            "name": name,
            "ok": false,
            "duration_ms": duration_ms,
            "error": error,
        });
        if let Some(r) = reason {
            obj["reason"] = serde_json::to_value(r).expect("reason serialises");
        }
        Self(obj)
    }
}

/// State held across rounds of a single turn's tool-call loop.
#[derive(Debug, Default)]
pub struct ToolRoundState {
    pub dispatched_hashes: HashSet<String>,
    pub summaries: Vec<ToolSummary>,
    pub rounds: usize,
}

impl ToolRoundState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool_calls_summary_values(&self) -> Vec<Value> {
        self.summaries.iter().map(|s| s.0.clone()).collect()
    }
}

/// Per-call dedupe check. `true` → caller should suppress this call;
/// the suppression event has already been emitted on the handle.
///
/// Mirrors Python's `state._dispatched_call_hashes` membership test in
/// `_driver.py:386-405`.
pub async fn dedupe_and_maybe_emit(
    handle: &Arc<TurnHandle>,
    state: &mut ToolRoundState,
    call: &ToolCall,
) -> bool {
    let h = call_hash(&call.name, &call.args);
    if state.dispatched_hashes.contains(&h) {
        handle
            .push_tool_event(ToolEvent::ToolError {
                turn_id: handle.turn_id.clone(),
                call_id: call.id.clone(),
                name: call.name.clone(),
                error: format!(
                    "duplicate tool call {:?} suppressed (same args as a \
                     prior call this turn)",
                    call.name
                ),
                reason: Some(ToolErrorReason::ToolCallTextDuplicate),
                duration_ms: 0.0,
            })
            .await;
        state.summaries.push(ToolSummary::err(
            &call.id,
            &call.name,
            0,
            "duplicate tool call suppressed",
            Some(ToolErrorReason::ToolCallTextDuplicate),
        ));
        return true;
    }
    state.dispatched_hashes.insert(h);
    false
}

/// Dispatch one tool call: tier-gate → route → MCP / internal call →
/// emit `ToolDispatched` + `ToolResult` | `ToolError` → record summary
/// → return the assistant-message addendum for the next round.
///
/// Returns the JSON-serialisable assistant tool-message reply that the
/// surrounding loop appends to its conversation history for the next
/// LLM call.
pub async fn run_one_tool(
    cfg: &'static Config,
    handle: &Arc<TurnHandle>,
    state: &mut ToolRoundState,
    device_tier: &str,
    registry: &Registry,
    call: &ToolCall,
) -> Value {
    handle
        .push_tool_event(ToolEvent::ToolDispatched {
            turn_id: handle.turn_id.clone(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            args: call.args.clone(),
        })
        .await;

    // Tier gate runs BEFORE dispatch so a blocked call never reaches
    // the bridge / runner. Matches the 0ms-ish rejection Python does
    // for the read_only tier. Phase 6 wires the registry-aware
    // destructive lookup so a `tool_use` tier denies destructive tools.
    if let Some(block) = check_tier_gate(device_tier, &call.name, registry) {
        handle
            .push_tool_event(ToolEvent::ToolError {
                turn_id: handle.turn_id.clone(),
                call_id: call.id.clone(),
                name: call.name.clone(),
                error: block.error.clone(),
                reason: Some(block.reason),
                duration_ms: 0.0,
            })
            .await;
        state.summaries.push(ToolSummary::err(
            &call.id,
            &call.name,
            0,
            &block.error,
            Some(block.reason),
        ));
        return tool_message(&call.id, &call.name, &format!("[tier_blocked] {}", block.error));
    }

    let started = Instant::now();
    let (result, reason): (Result<Value, wylde_shared::ipc::IpcError>, Option<ToolErrorReason>) =
        match dispatch::route(cfg, &call.name) {
            Route::McpExtension => (
                dispatch::call_mcp_extension(cfg, &call.name, call.args.clone()).await,
                None,
            ),
            Route::Internal => {
                let outcome =
                    dispatch::call_internal(cfg, registry, &call.name, device_tier, call.args.clone())
                        .await;
                match outcome.result {
                    Ok(v) => (Ok(v), None),
                    Err(e) => (Err(e.error), e.reason),
                }
            }
        };
    let elapsed_ms = duration_ms(started);

    match result {
        Ok(output) => {
            handle
                .push_tool_event(ToolEvent::ToolResult {
                    turn_id: handle.turn_id.clone(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                    duration_ms: elapsed_ms as f64,
                })
                .await;
            maybe_emit_memory_written(handle, call, &output).await;
            state
                .summaries
                .push(ToolSummary::ok(&call.id, &call.name, elapsed_ms));
            tool_message(&call.id, &call.name, &stringify_output(&output))
        }
        Err(err) => {
            let msg = format!("{}: {}", err.code, err.message);
            handle
                .push_tool_event(ToolEvent::ToolError {
                    turn_id: handle.turn_id.clone(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    error: msg.clone(),
                    reason,
                    duration_ms: elapsed_ms as f64,
                })
                .await;
            state.summaries.push(ToolSummary::err(
                &call.id,
                &call.name,
                elapsed_ms,
                &msg,
                reason,
            ));
            tool_message(&call.id, &call.name, &format!("[error] {msg}"))
        }
    }
}

fn duration_ms(started: Instant) -> u64 {
    let elapsed = started.elapsed();
    elapsed
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn tool_message(call_id: &str, name: &str, content: &str) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": call_id,
        "name": name,
        "content": content,
    })
}

/// Render a tool output payload to a string for the LLM history. JSON
/// values stringify via `serde_json`; plain strings pass through.
fn stringify_output(v: &Value) -> String {
    if let Value::String(s) = v {
        return s.clone();
    }
    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
}

/// If `call.name` is a memory-write tool and `output` carries a
/// `memory: {id, body, importance}` record, emit a structured
/// `MemoryWritten` event so the GUI can render auto-writes distinctly.
async fn maybe_emit_memory_written(
    handle: &Arc<TurnHandle>,
    call: &ToolCall,
    output: &Value,
) {
    if !MEMORY_WRITE_TOOL_IDS.iter().any(|id| *id == call.name) {
        return;
    }
    let Some(record) = output.get("memory").and_then(Value::as_object) else {
        return;
    };
    let memory_id = record
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if memory_id.is_empty() {
        return;
    }
    let body = record
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = truncate_preview(&body, 200);
    let importance = record
        .get("importance")
        .and_then(Value::as_f64);
    let scope = match call.name.as_str() {
        "memory_workspace_save" => MemoryWriteScope::Workspace,
        "memory_update" => {
            // memory_update preserves the original scope, threaded through
            // args. Mirrors Python `_tool_round.py:427`.
            match call.args.get("scope").and_then(Value::as_str) {
                Some("workspace") => MemoryWriteScope::Workspace,
                Some("short_term") => MemoryWriteScope::ShortTerm,
                _ => MemoryWriteScope::LongTerm,
            }
        }
        _ => MemoryWriteScope::LongTerm,
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    handle
        .push_tool_event(ToolEvent::MemoryWritten {
            turn_id: handle.turn_id.clone(),
            source: MemoryWriteSource::LlmTool,
            scope,
            memory_id,
            body,
            importance,
            timestamp_ms: now_ms,
            call_id: Some(call.id.clone()),
            tool: Some(call.name.clone()),
        })
        .await;
}

fn truncate_preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    let trimmed = out.trim_end().to_string();
    out.clear();
    out.push_str(&trimmed);
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::register_turn;
    use serde_json::json;

    #[test]
    fn normalise_device_tier_falls_back_to_default() {
        assert_eq!(normalise_device_tier(None), TIER_TOOL_USE);
        assert_eq!(normalise_device_tier(Some("")), TIER_TOOL_USE);
        assert_eq!(normalise_device_tier(Some("nonsense")), TIER_TOOL_USE);
    }

    #[test]
    fn normalise_device_tier_passes_known_values_through() {
        assert_eq!(normalise_device_tier(Some(TIER_READ_ONLY)), TIER_READ_ONLY);
        assert_eq!(normalise_device_tier(Some(TIER_TOOL_USE)), TIER_TOOL_USE);
        assert_eq!(
            normalise_device_tier(Some(TIER_DESTRUCTIVE)),
            TIER_DESTRUCTIVE
        );
    }

    #[test]
    fn check_tier_gate_blocks_read_only_tier() {
        let reg = Registry::empty();
        let block = check_tier_gate(TIER_READ_ONLY, "fs.read", &reg).expect("blocks");
        assert_eq!(block.reason, ToolErrorReason::TierReadOnly);
        assert!(block.error.contains("fs.read"));
    }

    #[test]
    fn check_tier_gate_allows_destructive_tier() {
        let reg = Registry::empty();
        assert!(check_tier_gate(TIER_DESTRUCTIVE, "fs.write", &reg).is_none());
    }

    #[test]
    fn check_tier_gate_allows_tool_use_for_non_destructive_tools() {
        // Phase 6 registry-aware gate: tool_use permits non-destructive
        // tools.
        let reg = Registry::default();
        assert!(check_tier_gate(TIER_TOOL_USE, "fs.read_file", &reg).is_none());
    }

    #[test]
    fn check_tier_gate_blocks_destructive_tool_on_tool_use() {
        let reg = Registry::default();
        let block =
            check_tier_gate(TIER_TOOL_USE, "fs.write_file", &reg).expect("destructive blocked");
        assert_eq!(block.reason, ToolErrorReason::TierReadOnly);
    }

    #[test]
    fn check_tier_gate_passes_unknown_tool_through_on_tool_use() {
        // Extension tools resolved via MCP namespace aren't in the
        // registry; they bypass the gate.
        let reg = Registry::empty();
        assert!(check_tier_gate(TIER_TOOL_USE, "webcrawler.scrape", &reg).is_none());
    }

    #[test]
    fn truncate_preview_passes_short_strings() {
        assert_eq!(truncate_preview("hi", 200), "hi");
    }

    #[test]
    fn truncate_preview_appends_ellipsis_when_truncated() {
        let s = "x".repeat(500);
        let p = truncate_preview(&s, 200);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), 201); // 200 + ellipsis
    }

    #[tokio::test]
    async fn dedupe_first_call_passes_second_call_suppressed() {
        // Serialise against the other tests that touch the process-global
        // turn registry + the resource registry `run_one_tool` dispatches
        // through (which `install_for_tests` elsewhere swaps). The crate's
        // existing `serial_test_guard()` is the lock those mutators already
        // hold — a separate `serial_test(registry)` pool wouldn't serialise
        // against them. This is the "registry" flaky cluster's real fix.
        let _g = crate::tooling::consent::serial_test_guard().await;
        let id = crate::state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());
        let mut state = ToolRoundState::new();
        let call = ToolCall {
            id: "call_1".into(),
            name: "fs.read".into(),
            args: json!({"path": "foo"}),
        };

        let first_dup = dedupe_and_maybe_emit(&handle, &mut state, &call).await;
        assert!(!first_dup, "first call should not be flagged duplicate");
        assert_eq!(state.dispatched_hashes.len(), 1);

        let second_dup = dedupe_and_maybe_emit(&handle, &mut state, &call).await;
        assert!(second_dup, "second call SHOULD be flagged duplicate");
        // The dup emits one tool_error event + one summary row.
        let tool_evs = handle.tool_events.lock().await;
        let dups: Vec<_> = tool_evs
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ToolEvent::ToolError {
                        reason: Some(ToolErrorReason::ToolCallTextDuplicate),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(dups.len(), 1);
        drop(tool_evs);
        assert_eq!(state.summaries.len(), 1);

        crate::state::remove_turn(&id);
    }

    #[tokio::test]
    async fn run_one_tool_now_routes_through_registry_with_ok_summary() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        let cfg: &'static Config = Box::leak(Box::new(Config::default_for_tests()));
        let id = crate::state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());
        let mut state = ToolRoundState::new();
        let reg = Registry::default();

        // time.now is a Phase 6 active tool — should succeed end-to-end.
        let call = ToolCall {
            id: "call_1".into(),
            name: "time.now".into(),
            args: json!({}),
        };
        let msg = run_one_tool(cfg, &handle, &mut state, TIER_TOOL_USE, &reg, &call).await;

        // Tool message is the JSON-stringified registry output.
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["tool_call_id"], "call_1");
        // Stringified output contains success status.
        assert!(msg["content"].as_str().unwrap().contains("success"));

        let evs = handle.tool_events.lock().await;
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], ToolEvent::ToolDispatched { .. }));
        assert!(matches!(evs[1], ToolEvent::ToolResult { .. }));
        drop(evs);
        assert_eq!(state.summaries.len(), 1);
        assert_eq!(state.summaries[0].0["ok"], true);
        crate::state::remove_turn(&id);
    }

    #[tokio::test]
    async fn run_one_tool_short_circuits_when_tier_blocks() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        let cfg: &'static Config = Box::leak(Box::new(Config::default_for_tests()));
        let id = crate::state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());
        let mut state = ToolRoundState::new();
        let reg = Registry::default();
        let call = ToolCall {
            id: "call_1".into(),
            name: "fs.read_file".into(),
            args: json!({}),
        };
        let msg = run_one_tool(cfg, &handle, &mut state, TIER_READ_ONLY, &reg, &call).await;
        assert!(msg["content"]
            .as_str()
            .unwrap()
            .contains("[tier_blocked]"));

        let evs = handle.tool_events.lock().await;
        assert!(matches!(evs[0], ToolEvent::ToolDispatched { .. }));
        assert!(matches!(
            evs[1],
            ToolEvent::ToolError {
                reason: Some(ToolErrorReason::TierReadOnly),
                ..
            }
        ));
        crate::state::remove_turn(&id);
    }

    #[tokio::test]
    async fn run_one_tool_blocks_destructive_tool_on_tool_use_tier() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        let cfg: &'static Config = Box::leak(Box::new(Config::default_for_tests()));
        let id = crate::state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());
        let mut state = ToolRoundState::new();
        let reg = Registry::default();
        // fs.write_file is destructive — tool_use tier denies.
        let call = ToolCall {
            id: "call_1".into(),
            name: "fs.write_file".into(),
            args: json!({"path": "foo", "content": "x"}),
        };
        let msg = run_one_tool(cfg, &handle, &mut state, TIER_TOOL_USE, &reg, &call).await;
        assert!(msg["content"]
            .as_str()
            .unwrap()
            .contains("[tier_blocked]"));
        assert_eq!(state.summaries.len(), 1);
        assert_eq!(state.summaries[0].0["ok"], false);
        crate::state::remove_turn(&id);
    }

    #[tokio::test]
    async fn run_one_tool_returns_phase_deferred_for_stub_entries() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        let cfg: &'static Config = Box::leak(Box::new(Config::default_for_tests()));
        let id = crate::state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());
        let mut state = ToolRoundState::new();
        let reg = Registry::default();
        // Pick a tool still on the deferred list. After Phase 7.B
        // moved the long_term memory tools and Phase 7.B-3 moved the
        // rag.* tools to active, `memory.workspace.save` is the
        // simplest still-deferred Phase 7 entry to assert against.
        let call = ToolCall {
            id: "call_1".into(),
            name: "memory.workspace.save".into(),
            args: json!({"body": "x"}),
        };
        let msg =
            run_one_tool(cfg, &handle, &mut state, TIER_DESTRUCTIVE, &reg, &call).await;
        assert!(msg["content"]
            .as_str()
            .unwrap()
            .contains("phase_7_deferred"));
        crate::state::remove_turn(&id);
    }

    #[test]
    fn tool_summary_ok_shape_matches_python() {
        let s = ToolSummary::ok("c1", "fs.read", 42);
        assert_eq!(s.0["call_id"], "c1");
        assert_eq!(s.0["name"], "fs.read");
        assert_eq!(s.0["ok"], true);
        assert_eq!(s.0["duration_ms"], 42);
        assert!(s.0.get("error").is_none());
    }

    #[test]
    fn tool_summary_err_carries_reason_when_present() {
        let s = ToolSummary::err("c1", "fs.read", 0, "boom", Some(ToolErrorReason::TierReadOnly));
        assert_eq!(s.0["ok"], false);
        assert_eq!(s.0["error"], "boom");
        assert_eq!(s.0["reason"], "tier_read_only");
    }
}
