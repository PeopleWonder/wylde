//! `chat.*` action handlers.
//!
//! ## Slice 5.A: `chat.run_turn`
//!
//! Single LLM round trip — no tool decode, no memory layer, no
//! conversation history persistence.
//!
//! ## Slice 5.B: streaming surface
//!
//! `chat.start_turn` returns a `turn_id` and spawns the turn-driving
//! task. The task drives [`send_action_stream`] against
//! `wylde-ollama`'s `ollama.chat_stream`, translates each chunk into a
//! `TurnEvent::Token`, and appends to the [`state::TurnHandle`]'s
//! per-turn event buffer.
//!
//! `chat.cancel` flips the per-turn cancel flag; the turn task
//! observes it between chunks and emits a final
//! `TurnEvent::TurnAborted` before marking done.
//!
//! `chat.stream_turn` / `chat.stream_tools` are STREAMING actions.
//! Each subscribes to a turn's event buffer and emits one IPC stream
//! chunk per event, exiting when the turn is done AND the subscriber
//! has drained past the buffer end.
//!
//! ## Slice 5.C: tool-call decode + dispatch
//!
//! The streaming driver now accumulates assistant text silently
//! (Option A — matches Python's `_driver.py:303-311`) and at
//! stream-complete runs the salvage parser. Recovered tool calls are
//! deduped, dispatched via [`crate::dispatch`], and the results fed
//! back into the next round. The loop bails out after
//! [`tool_round::MAX_TOOL_LOOPS`] rounds.
//!
//! `chat.run_turn` mirrors the same loop using the unary `ollama.chat`
//! action; both handlers populate `tool_calls_summary` from
//! [`tool_round::ToolRoundState::tool_calls_summary_values`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use wylde_shared::ipc::{self, IpcError, Reply, StreamSender};

use crate::config::Config;
use crate::events::{AbortReason, ToolErrorReason, ToolEvent, TurnEvent};
use crate::state::{self, TurnHandle};
use crate::turn::chat_options;
use crate::turn::context_gather;
use crate::turn::prompt;
use crate::turn::reasoning;
use crate::turn::salvage::{self, RecoveredCall, SalvageResult};
use crate::turn::tool_round::{self, ToolCall, ToolRoundState};
use crate::turn::workspace_context;

/// `chat.preview_context` — **Phase 1** of the concept-routing R2
/// curate-before-inject turn (concept-routing plan §4). Routes the turn's query
/// into concept space and returns the candidate set the GUI menu is built from,
/// **without** injecting or driving the LLM. The GUI then shows the menu (or
/// auto-applies the remembered selection) and sends the user-curated concept ids
/// on `chat.run_turn` (`curated_concepts`).
///
/// Reply: `{ conversation_id, routing_enabled, curate, candidates, inject_token_budget }`.
/// * `routing_enabled = false` ⇒ the master toggle is OFF: no menu, and
///   `chat.run_turn` proceeds exactly as today (`candidates` is `null`).
/// * `curate` mirrors `curate_before_inject` — when `false` the GUI auto-applies
///   the default-checked set without a blocking menu (but still never silent:
///   the first turn shows it; this is the per-conversation friction valve).
/// * `candidates` is `null` when nothing routed (raw-RAG fallback) or the
///   workspace is unreachable.
///
/// Purely additive + behaviour-safe: this verb only *reads*; it injects nothing.
pub async fn handle_preview_context(payload: Value) -> Reply {
    let user_message = match string_field(&payload, "user_message") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let conversation_id = optional_string(&payload, "conversation_id").unwrap_or_default();
    let workspace_id = optional_string(&payload, "workspace_id");
    let overrides = context_gather::TokenOverrides::from_payload(&payload);

    let cfg = wylde_concept_routing::RoutingConfig::current();
    if !cfg.enabled {
        // Toggle OFF — no preview, no routing. The GUI shows no menu and the
        // turn runs as today.
        return Reply::ok(json!({
            "conversation_id": conversation_id,
            "routing_enabled": false,
            "curate": false,
            "candidates": Value::Null,
            "inject_token_budget": cfg.inject_token_budget,
        }));
    }

    let candidates = context_gather::preview(
        workspace_id.as_deref(),
        &user_message,
        &conversation_id,
        &overrides,
    )
    .await;

    let candidates_json = candidates
        .as_ref()
        .and_then(|c| serde_json::to_value(c).ok())
        .unwrap_or(Value::Null);

    Reply::ok(json!({
        "conversation_id": conversation_id,
        "routing_enabled": true,
        "curate": cfg.curate_before_inject,
        "candidates": candidates_json,
        "inject_token_budget": cfg.inject_token_budget,
    }))
}

/// `chat.run_turn` — synchronous chat turn with the 5.C tool-round
/// loop. Drives the LLM → salvage → dispatch cycle up to
/// [`tool_round::MAX_TOOL_LOOPS`] rounds, then returns the final
/// message + a [`tool_round::ToolSummary`] list.
///
/// **Concept-routing R2 (Phase 2):** the `curated_concepts` array — the user's
/// curate-before-inject choices from `chat.preview_context` — rides on the
/// payload and is parsed into [`context_gather::TokenOverrides`]; when the master
/// toggle is ON, the gather Augment-injects exactly those concepts (boundary
/// blurb + member snippets) alongside RAG. Absent ⇒ no injection; toggle OFF ⇒
/// the list is ignored entirely (byte-identical to today).
pub async fn handle_run_turn(payload: Value) -> Reply {
    let cfg = Config::get();

    let user_message = match string_field(&payload, "user_message") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let conversation_id = match string_field(&payload, "conversation_id") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let model = resolve_model(&payload, cfg);
    if model.is_empty() {
        return Reply::err(IpcError::new(
            "bad_request",
            "model is required (none provided and WYLDE_DEFAULT_MODEL is unset)",
        ));
    }

    let turn_id = optional_string(&payload, "turn_id").unwrap_or_else(state::new_turn_id);
    let device_tier = optional_string(&payload, "device_tier").unwrap_or_default();
    let workspace_id = optional_string(&payload, "workspace_id");
    let normalised_tier = tool_round::normalise_device_tier(if device_tier.is_empty() {
        None
    } else {
        Some(device_tier.as_str())
    });
    // Agentic-reasoning S1: depth rides the payload (payload → config →
    // Fast). v1 scopes Deep to the STREAMING driver only (plan §1) — the
    // unary path always runs Fast; a Deep request is honestly flagged in
    // the reply via `depth_ignored` so extension/N8N callers aren't
    // silently degraded. Fast (the only value today's callers produce) is
    // a no-op: no reply field, byte-identical.
    let depth = reasoning::resolve_depth(&payload);

    tracing::debug!(
        turn_id = %turn_id,
        conversation_id = %conversation_id,
        model = %model,
        device_tier = %normalised_tier,
        depth = depth.as_str(),
        "harness: run_turn entered"
    );

    // Register a handle so dispatch can emit tool events into the
    // standard per-turn buffer (a subscriber that polls stream_tools
    // mid-run_turn still sees them).
    let handle = state::register_turn(turn_id.clone(), conversation_id.clone());

    // Gather the turn's context (Thought Bubble System Slice G): detect symbol
    // + anchor references in the prompt, pull their structural code-graph
    // context, fold in the user profile + short-term memory + workspace prompt
    // block, apply the OI-8 token budget, and render the named slots. When the
    // workspaces service is down the gather degrades to base context and flags
    // the turn so the response carries a one-line notice. The composer's
    // per-message ✕/↺ choices (Slices F + M) arrive as token overrides.
    let overrides = context_gather::TokenOverrides::from_payload(&payload);
    // The gather-slot eviction budget derives from the model's effective
    // num_ctx (B5) — the base prompt + user message are fixed costs the
    // slots must leave room for.
    let base_prompt = base_system_prompt(&model);
    let slot_budget = chat_options::slot_budget(&model, &base_prompt, &user_message).await;
    let gathered = context_gather::gather(
        workspace_id.as_deref(),
        &user_message,
        &conversation_id,
        &overrides,
        slot_budget,
    )
    .await;
    log_tier7_degrade(&gathered, &conversation_id, slot_budget);
    // Replay the honest gather activity log (chat-processing-indicator, full
    // visibility): each retrieval / routing / injection / memory step the
    // gather actually performed, as ordered `Step` events the GUI dropdown
    // surfaces. Empty on a plain turn that gathered nothing.
    for step in &gathered.steps {
        handle
            .push_turn_event(TurnEvent::Step {
                turn_id: turn_id.clone(),
                stage: step.stage,
                summary: step.summary.clone(),
                detail: step.detail.clone(),
            })
            .await;
    }
    let mut messages = initial_messages(
        base_prompt,
        &gathered.history,
        &user_message,
        &gathered.system_slots,
    );
    let tools = tools_payload();
    // The user's per-model inference overrides (Settings → Ollama) ride
    // every round's request as Ollama `options` (B5).
    let options = chat_options::chat_options(&model);
    let mut round_state = ToolRoundState::new();
    let alias_map = build_alias_map();
    let mut final_text = String::new();
    let mut completed_naturally = false;
    let mut abort_reason: Option<AbortReason> = None;
    let mut abort_error: Option<String> = None;

    for round in 0..tool_round::MAX_TOOL_LOOPS {
        round_state.rounds = round + 1;

        let mut body = json!({
            "model": model,
            "messages": messages,
            "tools": tools,
            "priority": cfg.default_chat_priority,
            "stream": false,
            "keep_alive": "24h",
        });
        if !options.is_empty() {
            body["options"] = Value::Object(options.clone());
        }

        let upstream = match ipc::call_action(&cfg.ollama_service, "ollama.chat", body).await {
            Ok(v) => v,
            Err(e) => {
                abort_reason = Some(AbortReason::Error);
                abort_error = Some(format!("{}: {}", e.code, e.message));
                break;
            }
        };
        let raw_text = extract_assistant_content(&upstream);
        let salvage_result = salvage::extract_tool_calls_from_content(&raw_text, &alias_map);
        final_text = salvage_result.cleaned_text.clone();
        emit_unrecognised(&handle, &salvage_result).await;

        // Native path first (capable models), salvage as fallback (small
        // models that emit the call as content). Per-turn dedupe in the
        // round loop coalesces any overlap.
        let native = native_tool_calls(&extract_native_tool_calls(&upstream), &alias_map);
        let mut calls = native;
        calls.extend(recovered_to_calls(&salvage_result));
        if calls.is_empty() {
            completed_naturally = true;
            break;
        }

        // Append the assistant message that carried the tool calls so
        // the next round has the right context.
        messages.push(json!({
            "role": "assistant",
            "content": salvage_result.cleaned_text,
            "tool_calls": tool_calls_wire(&calls),
        }));

        for call in &calls {
            if tool_round::dedupe_and_maybe_emit(&handle, &mut round_state, call).await {
                continue;
            }
            let tool_msg = tool_round::run_one_tool(
                cfg,
                &handle,
                &mut round_state,
                normalised_tier,
                crate::tooling::registry::global(),
                call,
            )
            .await;
            messages.push(tool_msg);
        }
    }

    let summary = round_state.tool_calls_summary_values();
    if !completed_naturally && abort_reason.is_none() {
        abort_reason = Some(AbortReason::ToolLoopLimit);
        abort_error = Some(format!(
            "exceeded {} tool-call iterations without a final response",
            tool_round::MAX_TOOL_LOOPS
        ));
    }
    let aborted_now = abort_reason.is_some();

    // Drop the handle slot — run_turn is fully synchronous, no
    // subscribers can race after this point.
    handle.mark_done();
    state::remove_turn(&turn_id);

    // Post-turn hooks — only on a naturally-completed turn (not an abort).
    if completed_naturally {
        run_post_turn_hooks(
            &conversation_id,
            workspace_id.as_deref(),
            &user_message,
            &final_text,
        );
    }

    // Surface the graceful-degradation notice when the workspaces service
    // was requested but unreachable (scope v2 §7.5).
    let final_text = workspace_context::apply_degraded_notice(final_text, gathered.degraded);

    let mut reply = json!({
        "turn_id": turn_id,
        "conversation_id": conversation_id,
        "final_message": final_text,
        "tool_calls_summary": summary,
        "aborted": aborted_now,
        "abort_reason": abort_reason
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        "abort_error": abort_error.map(Value::String).unwrap_or(Value::Null),
    });
    // Only a planning-tier request grows the reply — the everyday Fast
    // reply shape is untouched (identity guard).
    if depth.plans() {
        reply["depth_ignored"] = json!(true);
    }
    Reply::ok(reply)
}

/// `chat.complete` — single-shot, narrow completion endpoint for
/// extensions (Wylde_Study S2a).
///
/// Deliberately *not* the agent loop. Where [`handle_run_turn`] injects
/// the full agent persona + tool catalog via [`initial_messages`] and
/// drives a salvage/dispatch loop, `chat.complete` sends exactly one
/// `{role: "user"}` message — no system prompt, no `tools` field, no
/// tool decode, no conversation history. It routes through the *same*
/// underlying pipeline as `chat.run_turn` (the `ollama.chat` IPC action
/// on `cfg.ollama_service`), so the wylde-ollama VRAM broker lease,
/// model resolution, and priority all apply identically — only the
/// message construction is stripped down.
///
/// Accepted args:
/// * `prompt` (required) — the user message.
/// * `model` (optional) — defaults to `WYLDE_DEFAULT_MODEL`.
/// * `max_tokens` (optional) — forwarded as Ollama `options.num_predict`.
///
/// Returns `{text, model_used, tokens_used, prompt_tokens,
/// completion_tokens}`. `tokens_used` is the sum of prompt + completion
/// counts the backend reports; the breakdown is included for callers
/// that want it (the Python Study handler tracks them separately).
pub async fn handle_complete(payload: Value) -> Reply {
    let cfg = Config::get();

    let prompt = match string_field(&payload, "prompt") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let model = resolve_model(&payload, cfg);
    if model.is_empty() {
        return Reply::err(IpcError::new(
            "bad_request",
            "model is required (none provided and WYLDE_DEFAULT_MODEL is unset)",
        ));
    }

    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "priority": cfg.default_chat_priority,
        "stream": false,
        "keep_alive": "24h",
    });
    // The user's per-model inference overrides (B5), then the narrow
    // `max_tokens` knob mapped onto Ollama's generation option — the
    // explicit per-call value beats a stored num_predict override. A
    // non-positive value is ignored (let the backend pick its default).
    let mut options = chat_options::chat_options(&model);
    if let Some(max) = payload.get("max_tokens").and_then(Value::as_i64) {
        if max > 0 {
            options.insert("num_predict".to_owned(), json!(max));
        }
    }
    if !options.is_empty() {
        body["options"] = Value::Object(options);
    }

    match ipc::call_action(&cfg.ollama_service, "ollama.chat", body).await {
        Ok(upstream) => {
            let text = extract_assistant_content(&upstream);
            let model_used = upstream
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&model)
                .to_owned();
            let prompt_tokens = upstream
                .get("prompt_eval_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let completion_tokens = upstream
                .get("eval_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Reply::ok(json!({
                "text": text,
                "model_used": model_used,
                "tokens_used": prompt_tokens + completion_tokens,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
            }))
        }
        Err(e) => Reply::err(IpcError::new(
            "chat_failed",
            format!("{}: {}", e.code, e.message),
        )),
    }
}

/// `chat.start_turn` — non-blocking kick-off (5.B). Validates inputs,
/// registers a turn handle, spawns the driving task, and returns the
/// turn id immediately.
pub async fn handle_start_turn(payload: Value) -> Reply {
    let cfg = Config::get();

    let user_message = match string_field(&payload, "user_message") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let conversation_id = match string_field(&payload, "conversation_id") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let model = resolve_model(&payload, cfg);
    if model.is_empty() {
        return Reply::err(IpcError::new(
            "bad_request",
            "model is required (none provided and WYLDE_DEFAULT_MODEL is unset)",
        ));
    }
    let device_tier = optional_string(&payload, "device_tier").unwrap_or_default();
    let normalised_tier = tool_round::normalise_device_tier(if device_tier.is_empty() {
        None
    } else {
        Some(device_tier.as_str())
    });

    let turn_id = optional_string(&payload, "turn_id").unwrap_or_else(state::new_turn_id);
    let workspace_id = optional_string(&payload, "workspace_id");
    // Agentic-reasoning: the GUI's fast/deep pill rides the wire as
    // `depth`; resolved payload → config → Fast. Since S3 the streaming
    // driver ACTS on it — a Deep turn with the master toggle on runs the
    // PLAN phase before the ReAct loop. With the toggle off (default) or
    // depth Fast the gate stays closed and the turn is byte-identical.
    let depth = reasoning::resolve_depth(&payload);
    tracing::debug!(
        turn_id = %turn_id,
        depth = depth.as_str(),
        gate_open = reasoning::deep_gate_open(depth),
        "harness: start_turn depth resolved"
    );
    let handle = state::register_turn(turn_id.clone(), conversation_id.clone());

    let drive_handle = Arc::clone(&handle);
    let drive_turn_id = turn_id.clone();
    let drive_conversation_id = conversation_id.clone();
    let drive_model = model.clone();
    let drive_user_message = user_message.clone();
    let drive_tier = normalised_tier.to_owned();
    let drive_overrides = context_gather::TokenOverrides::from_payload(&payload);

    tokio::spawn(async move {
        drive_streaming_turn(
            cfg,
            drive_handle,
            drive_turn_id,
            drive_conversation_id,
            drive_user_message,
            drive_model,
            drive_tier,
            workspace_id,
            drive_overrides,
            depth,
        )
        .await;
    });

    Reply::ok(json!({
        "turn_id": turn_id,
        "conversation_id": conversation_id,
    }))
}

/// `chat.cancel` — flips the per-turn cancel flag (5.B).
pub async fn handle_cancel(payload: Value) -> Reply {
    let turn_id = match string_field(&payload, "turn_id") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let cancelled = state::cancel_turn(&turn_id);
    Reply::ok(json!({
        "turn_id": turn_id,
        "cancelled": cancelled,
    }))
}

/// `chat.stream_turn` — STREAMING handler (5.B). Emits one chunk per
/// user-facing event from cursor=0.
pub async fn handle_stream_turn(payload: Value, sender: StreamSender) {
    let turn_id = match string_field(&payload, "turn_id") {
        Ok(s) => s,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };
    let Some(handle) = state::get_turn(&turn_id) else {
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(IpcError::new(
                "not_found",
                format!("turn {turn_id:?} not found"),
            )))
            .await;
        return;
    };
    stream_events(handle, sender, Source::Turn).await;
}

/// `chat.stream_tools` — STREAMING handler (5.C). Emits one chunk per
/// tool-activity event from cursor=0.
pub async fn handle_stream_tools(payload: Value, sender: StreamSender) {
    let turn_id = match string_field(&payload, "turn_id") {
        Ok(s) => s,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };
    let Some(handle) = state::get_turn(&turn_id) else {
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(IpcError::new(
                "not_found",
                format!("turn {turn_id:?} not found"),
            )))
            .await;
        return;
    };
    stream_events(handle, sender, Source::Tool).await;
}

#[derive(Clone, Copy)]
enum Source {
    Turn,
    Tool,
}

async fn stream_events(handle: Arc<TurnHandle>, sender: StreamSender, source: Source) {
    let mut cursor: usize = 0;

    loop {
        let wake = handle.notify.notified();
        tokio::pin!(wake);

        let new_chunks: Vec<Value> = match source {
            Source::Turn => {
                let buf = handle.turn_events.lock().await;
                buf.iter()
                    .skip(cursor)
                    .map(|ev| serde_json::to_value(ev).expect("TurnEvent serialises"))
                    .collect()
            }
            Source::Tool => {
                let buf = handle.tool_events.lock().await;
                buf.iter()
                    .skip(cursor)
                    .map(|ev| serde_json::to_value(ev).expect("ToolEvent serialises"))
                    .collect()
            }
        };

        for chunk in new_chunks {
            cursor += 1;
            if sender.send(Ok(chunk)).await.is_err() {
                return;
            }
        }

        if handle.is_done() {
            return;
        }

        wake.await;
    }
}

/// How many newly-streamed tokens accrue before the driver emits a
/// running [`TurnEvent::Usage`] tick (chat-processing-indicator). Coalesces
/// Ollama's per-token frames into a few UI updates per turn rather than one
/// IPC event per token.
const USAGE_TICK_EVERY: u64 = 16;

/// Streaming turn driver — the spawned task `chat.start_turn` kicks
/// off. Each round opens a fresh `ollama.chat_stream`, accumulates
/// assistant text silently (Option A), salvages tool calls at
/// stream-complete, dispatches them, and feeds the results back into
/// the next round. Bails out at [`tool_round::MAX_TOOL_LOOPS`] or
/// when there are no more tool calls.
#[allow(clippy::too_many_arguments)] // turn-driver fan-out; grouping into a
                                     // struct would only move the noise. Slice G added `conversation_id`; Slice M
                                     // added the composer's per-message token overrides; reasoning S3 added `depth`.
async fn drive_streaming_turn(
    cfg: &'static Config,
    handle: Arc<TurnHandle>,
    turn_id: String,
    conversation_id: String,
    user_message: String,
    model: String,
    device_tier: String,
    workspace_id: Option<String>,
    overrides: context_gather::TokenOverrides,
    depth: reasoning::Depth,
) {
    // Status line (chat-processing-indicator): the gather below does RAG
    // retrieval + concept routing/injection and can take a beat, so flag
    // the phase before it starts. Purely informational; the GUI animates a
    // Claude-style status from these and degrades gracefully if absent.
    handle
        .push_turn_event(TurnEvent::Phase {
            turn_id: turn_id.clone(),
            phase: crate::events::TurnPhase::GatheringContext,
        })
        .await;

    // Gather the turn's context (Slice G) — see `handle_run_turn` for the flow.
    let base_prompt = base_system_prompt(&model);
    let slot_budget = chat_options::slot_budget(&model, &base_prompt, &user_message).await;
    let gathered = context_gather::gather(
        workspace_id.as_deref(),
        &user_message,
        &conversation_id,
        &overrides,
        slot_budget,
    )
    .await;
    log_tier7_degrade(&gathered, &conversation_id, slot_budget);
    // Replay the honest gather activity log (chat-processing-indicator, full
    // visibility): each retrieval / routing / injection / memory step the
    // gather actually performed, as ordered `Step` events the GUI dropdown
    // surfaces. Empty on a plain turn that gathered nothing.
    for step in &gathered.steps {
        handle
            .push_turn_event(TurnEvent::Step {
                turn_id: turn_id.clone(),
                stage: step.stage,
                summary: step.summary.clone(),
                detail: step.detail.clone(),
            })
            .await;
    }
    // Built before the plan seam so the planner validates tool names
    // against the exact alias map the executor dispatches with (a pure
    // registry read — order is behaviour-neutral).
    let alias_map = build_alias_map();

    // Agentic-reasoning S3, seam 1 (post-gather): on a Deep turn with the
    // master toggle on, run the PLAN phase — one reasoner call grounded in
    // the turn's own routed concepts / IS-NOT exclusions / lessons. Every
    // skip or failure path yields `None` and the loop below runs verbatim
    // (gate closed ⇒ byte-identical fast path; planner failure ⇒ visible
    // notice + plain ReAct).
    let mut reasoning_state = reasoning::maybe_plan(
        cfg,
        &handle,
        &turn_id,
        depth,
        workspace_id.as_deref(),
        &user_message,
        &gathered,
        &alias_map,
    )
    .await;

    // Agentic-reasoning S4b: on a FAST turn with reasoning enabled +
    // auto_escalate, arm the hard-tool-failure watch (the maintainer's narrowed
    // identity contract: byte-identical EXCEPT after ≥2 hard failures).
    // Pure counting — below the threshold nothing is emitted or changed;
    // toggle off ⇒ `None` and the fast path carries no watch at all.
    let mut escalation_watch = reasoning::arm_escalation(depth);

    let mut messages = initial_messages(
        base_prompt,
        &gathered.history,
        &user_message,
        &gathered.system_slots,
    );
    let tools = tools_payload();
    // Per-model inference overrides ride every round's request (B5).
    let options = chat_options::chat_options(&model);
    let mut round_state = ToolRoundState::new();

    let normalised_tier = tool_round::normalise_device_tier(if device_tier.is_empty() {
        None
    } else {
        Some(device_tier.as_str())
    });

    // Turn-level token meter (chat-processing-indicator). Folds each
    // round's exact Ollama counts so a multi-round (tool-using) turn shows
    // its cumulative usage. `turn_prompt` sums input tokens processed
    // across rounds (the context is re-sent each round); `turn_completion`
    // sums generated tokens. A Deep turn's PLAN call is part of the turn's
    // honest cost, so its counts seed the meter.
    let mut turn_prompt: u64 = 0;
    let mut turn_completion: u64 = 0;
    if let Some(rs) = &reasoning_state {
        turn_prompt += rs.plan_prompt_tokens;
        turn_completion += rs.plan_completion_tokens;
    }

    for round in 0..tool_round::MAX_TOOL_LOOPS {
        round_state.rounds = round + 1;

        if handle.is_cancelled() {
            emit_aborted(&handle, &turn_id, AbortReason::Cancelled, None).await;
            handle.mark_done();
            schedule_eviction(turn_id.clone());
            return;
        }

        // The LLM is about to generate this round.
        handle
            .push_turn_event(TurnEvent::Phase {
                turn_id: turn_id.clone(),
                phase: crate::events::TurnPhase::Generating,
            })
            .await;

        // Agentic-reasoning S3, seam 2 (round-entry): when a plan exists,
        // the next ready step's guidance rides the MESSAGE TAIL as a user
        // message (append-only — the KV prefix over system + history
        // survives, plan §9 R5). The model still emits the actual tool
        // call; dispatch authority is unchanged.
        if let Some(rs) = &mut reasoning_state {
            if let Some(guidance) = rs.begin_round() {
                messages.push(guidance);
            }
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
            "tools": tools,
            "priority": cfg.default_chat_priority,
            "stream": true,
            "keep_alive": "24h",
        });
        if !options.is_empty() {
            body["options"] = Value::Object(options.clone());
        }

        let mut stream = ipc::send_action_stream(&cfg.ollama_service, "ollama.chat_stream", body);
        let mut accumulated = String::new();
        // Peels inline `<think>…</think>` reasoning (DeepSeek-R1-style
        // reasoners) out of the streamed content so the trace goes to the
        // Thinking dropdown, not the answer. Fast models emit no `<think>`
        // ⇒ pure pass-through, byte-identical to the pre-P1 path.
        let mut think_splitter = super::think_stream::ThinkSplitter::new();
        // Native `message.tool_calls` may arrive on any chunk (Ollama
        // typically emits them whole on the final chunk); accumulate
        // across the stream and decode after it completes.
        let mut native_raw: Vec<Value> = Vec::new();
        let mut errored: Option<String> = None;
        let mut cancelled_mid_round = false;

        // Live token tick (chat-processing-indicator). Ollama streams ≈ one
        // content frame per generated token, so a running frame count is a
        // good-enough live meter; the final `done` frame's `eval_count` /
        // `prompt_eval_count` then give the authoritative totals.
        let mut frame_tokens: u64 = 0;
        let mut emitted_at: u64 = 0;
        let mut round_prompt: Option<u64> = None;
        let mut round_completion: Option<u64> = None;

        loop {
            tokio::select! {
                _ = handle.cancel.notified() => {
                    cancelled_mid_round = true;
                    break;
                }
                next = stream.next() => {
                    match next {
                        None => break,
                        Some(Err(e)) => {
                            errored = Some(format!("{}: {}", e.code, e.message));
                            break;
                        }
                        Some(Ok(chunk)) => {
                            if let Some(piece) = extract_chunk_content(&chunk) {
                                // Frame count keys off the RAW content piece,
                                // exactly as before — the token meter is
                                // unchanged for the fast path. The splitter then
                                // routes inline `<think>` to the Thinking stream
                                // and keeps only the answer body in `accumulated`.
                                if !piece.is_empty() {
                                    frame_tokens += 1;
                                }
                                let split = think_splitter.push(&piece);
                                accumulated.push_str(&split.answer);
                                if !split.thinking.is_empty() {
                                    handle
                                        .push_turn_event(TurnEvent::Thinking {
                                            turn_id: turn_id.clone(),
                                            text: split.thinking,
                                        })
                                        .await;
                                }
                            }
                            // Forward any reasoning delta (native thinking-API
                            // models expose it on `message.thinking`, separate
                            // from the inline `<think>` handled above).
                            if let Some(thought) = extract_chunk_thinking(&chunk) {
                                handle
                                    .push_turn_event(TurnEvent::Thinking {
                                        turn_id: turn_id.clone(),
                                        text: thought,
                                    })
                                    .await;
                            }
                            native_raw.extend(extract_native_tool_calls(&chunk));
                            // The final `done` frame carries the exact counts.
                            if let Some(p) = chunk.get("prompt_eval_count").and_then(Value::as_u64) {
                                round_prompt = Some(p);
                            }
                            if let Some(c) = chunk.get("eval_count").and_then(Value::as_u64) {
                                round_completion = Some(c);
                            }
                            // Throttled running tick — coalesces hundreds of
                            // per-token frames into a handful of UI updates.
                            if frame_tokens - emitted_at >= USAGE_TICK_EVERY {
                                emitted_at = frame_tokens;
                                handle
                                    .push_turn_event(TurnEvent::Usage {
                                        turn_id: turn_id.clone(),
                                        prompt_tokens: round_prompt.map(|p| turn_prompt + p),
                                        completion_tokens: turn_completion + frame_tokens,
                                        done: false,
                                    })
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        drop(stream);

        // Flush any bytes the splitter held back (a marker fragment or an
        // unterminated `<think>`). On the fast path this is at most a trailing
        // `<`-prefixed fragment, flushed verbatim into `accumulated` — so the
        // reassembled answer is byte-identical to the raw content.
        let tail = think_splitter.finish();
        accumulated.push_str(&tail.answer);
        if !tail.thinking.is_empty() {
            handle
                .push_turn_event(TurnEvent::Thinking {
                    turn_id: turn_id.clone(),
                    text: tail.thinking,
                })
                .await;
        }

        // Fold this round's authoritative counts into the turn meter
        // (falling back to the live frame count if Ollama omitted them).
        turn_prompt += round_prompt.unwrap_or(0);
        turn_completion += round_completion.unwrap_or(frame_tokens);

        if cancelled_mid_round || handle.is_cancelled() {
            emit_aborted(&handle, &turn_id, AbortReason::Cancelled, None).await;
            handle.mark_done();
            schedule_eviction(turn_id);
            return;
        }
        if let Some(err) = errored {
            emit_aborted(&handle, &turn_id, AbortReason::Error, Some(err)).await;
            handle.mark_done();
            schedule_eviction(turn_id);
            return;
        }

        // Salvage tool calls from the assistant content. Option A:
        // text is emitted post-salvage so the JSON never leaks to the
        // user-facing stream.
        let salvage_result = salvage::extract_tool_calls_from_content(&accumulated, &alias_map);
        let final_text = salvage_result.cleaned_text.clone();
        emit_unrecognised(&handle, &salvage_result).await;

        // Native path first (capable models), salvage as fallback (small
        // models that emit the call as content). Per-turn dedupe in the
        // round loop coalesces any overlap.
        let mut calls = native_tool_calls(&native_raw, &alias_map);
        calls.extend(recovered_to_calls(&salvage_result));

        if calls.is_empty() {
            // Agentic-reasoning S5, seam 4 (pre-finalize): on a
            // plan-guided turn's natural completion, run the REFLECT
            // critique — gate-checked, at most once per turn, entirely
            // fail-soft (any reflection failure finalizes verbatim). A
            // critique that finds gaps buys ONE extra EXECUTE round: the
            // draft joins the tail as an assistant message, the gaps as a
            // user message, and the loop continues — only when rounds
            // remain, so the loop cap stays authoritative. The draft is
            // never emitted to the user; the gap round's answer replaces
            // it. Fast turns never construct the state, so this block is
            // unreachable on the fast path (identity).
            if let Some(rs) = &mut reasoning_state {
                let flow = reasoning::maybe_reflect(
                    cfg,
                    &handle,
                    &turn_id,
                    rs,
                    round_state.dispatched_hashes.len(),
                    &final_text,
                    round + 1 < tool_round::MAX_TOOL_LOOPS,
                )
                .await;
                // The critique call is part of the turn's honest cost.
                turn_prompt += flow.prompt_tokens;
                turn_completion += flow.completion_tokens;
                if let Some(gap_message) = flow.gap_message {
                    messages.push(json!({
                        "role": "assistant",
                        "content": final_text.clone(),
                    }));
                    messages.push(gap_message);
                    continue;
                }
            }
            // No tool calls — emit the cleaned text as a single Token
            // event (bulk emit; mirrors Python's no-stream-token path
            // in `_driver.py:416-419`) and finish. Prefix the graceful-
            // degradation notice when the workspaces service was requested
            // but unreachable (scope v2 §7.5).
            //
            // Post-turn hooks fire on this natural completion with the
            // PRE-notice text (the degraded banner is UI chrome, not
            // exchange content). The streaming path previously had NO
            // post-turn reflection at all — only run_turn did; B14
            // closes that gap for both drivers.
            run_post_turn_hooks(
                &conversation_id,
                workspace_id.as_deref(),
                &user_message,
                &final_text,
            );
            let final_text =
                workspace_context::apply_degraded_notice(final_text, gathered.degraded);
            if !final_text.is_empty() {
                handle
                    .push_turn_event(TurnEvent::Token {
                        turn_id: turn_id.clone(),
                        text: final_text.clone(),
                    })
                    .await;
            }
            // Authoritative end-of-turn token totals.
            emit_final_usage(&handle, &turn_id, turn_prompt, turn_completion).await;
            handle
                .push_turn_event(TurnEvent::TurnComplete {
                    turn_id: turn_id.clone(),
                    final_message: final_text,
                })
                .await;
            handle.mark_done();
            schedule_eviction(turn_id);
            return;
        }

        // Tool calls present — the model wants to act before answering.
        handle
            .push_turn_event(TurnEvent::Phase {
                turn_id: turn_id.clone(),
                phase: crate::events::TurnPhase::RunningTools,
            })
            .await;

        // Emit any pre-call text (the "let me look that up" mid-stream),
        // then dispatch each call.
        if !final_text.is_empty() {
            handle
                .push_turn_event(TurnEvent::Token {
                    turn_id: turn_id.clone(),
                    text: final_text.clone(),
                })
                .await;
        }

        messages.push(json!({
            "role": "assistant",
            "content": final_text.clone(),
            "tool_calls": tool_calls_wire(&calls),
        }));

        // Collected only on a planned (Deep) turn — feeds the plan's
        // `${step.output…}` placeholder resolution (seam 3) and the S4
        // outcome check below.
        let mut round_results: Vec<(String, String)> = Vec::new();
        for call in &calls {
            if handle.is_cancelled() {
                emit_aborted(&handle, &turn_id, AbortReason::Cancelled, None).await;
                handle.mark_done();
                schedule_eviction(turn_id);
                return;
            }
            if tool_round::dedupe_and_maybe_emit(&handle, &mut round_state, call).await {
                continue;
            }
            let tool_msg = tool_round::run_one_tool(
                cfg,
                &handle,
                &mut round_state,
                normalised_tier,
                crate::tooling::registry::global(),
                call,
            )
            .await;
            if reasoning_state.is_some() {
                if let Some(content) = tool_msg.get("content").and_then(Value::as_str) {
                    // Record the CANONICAL tool id so the plan step's
                    // outcome check binds to a dispatch of the step's own
                    // tool (the plan stores canonical ids; the model emits
                    // dotted/aliased names). Without this, `finish_round`'s
                    // tool-match would never fire and every step would look
                    // un-executed. Falls back to the raw name for a tool the
                    // alias map doesn't know.
                    let canonical = alias_map
                        .get(&call.name)
                        .cloned()
                        .unwrap_or_else(|| call.name.clone());
                    round_results.push((canonical, content.to_owned()));
                }
            } else if let Some(watch) = &mut escalation_watch {
                // S4b: pure hard-failure counting on a watched Fast turn
                // — no events, no behaviour change below the threshold.
                if let Some(content) = tool_msg.get("content").and_then(Value::as_str) {
                    watch.observe(&call.name, &call.args, content);
                }
            }
            messages.push(tool_msg);
        }
        if let Some(rs) = &mut reasoning_state {
            // Agentic-reasoning S4, seam 3b (post-dispatch): record the
            // round's results, then CHECK the realised outcome against the
            // step's declared expectation — L0/L1 pure, L2 gated, replan
            // budget-gated (cheap detect / expensive respond). Everything
            // in the check is fail-soft except a planner-declared `abort`
            // action, which ends the turn cleanly. Replans run at the
            // state's tier (the turn's depth, or the escalation tier on
            // an S4b-escalated Fast turn).
            let completion = rs.finish_round(&round_results);
            let flow = reasoning::surprise::check_and_maybe_replan(
                cfg, &handle, &turn_id, rs.tier, &model, &alias_map, rs, completion,
            )
            .await;
            // The L2 / replan calls are part of the turn's honest cost.
            turn_prompt += flow.prompt_tokens;
            turn_completion += flow.completion_tokens;
            if let Some(detail) = flow.abort {
                emit_aborted(
                    &handle,
                    &turn_id,
                    AbortReason::PlanPrecondition,
                    Some(detail),
                )
                .await;
                handle.mark_done();
                schedule_eviction(turn_id);
                return;
            }
        } else if let Some(watch) = &mut escalation_watch {
            // Agentic-reasoning S4b: the 2nd hard tool failure escalates
            // this Fast turn to planning (the maintainer's narrowed identity
            // contract). One-shot — the watch is disarmed whether the
            // escalated PLAN succeeds or fail-softs to plain ReAct.
            if watch.should_escalate() {
                reasoning_state = reasoning::maybe_escalate(
                    cfg,
                    &handle,
                    &turn_id,
                    workspace_id.as_deref(),
                    &user_message,
                    &gathered,
                    &alias_map,
                    watch,
                )
                .await;
                if let Some(rs) = &reasoning_state {
                    // The escalated PLAN call is part of the turn's cost.
                    turn_prompt += rs.plan_prompt_tokens;
                    turn_completion += rs.plan_completion_tokens;
                }
                escalation_watch = None;
            }
        }
    }

    // Hit the loop cap.
    emit_aborted(
        &handle,
        &turn_id,
        AbortReason::ToolLoopLimit,
        Some(format!(
            "exceeded {} tool-call iterations without a final response",
            tool_round::MAX_TOOL_LOOPS
        )),
    )
    .await;
    handle.mark_done();
    schedule_eviction(turn_id);
}

/// Post-turn hooks, run only on natural completion:
///
/// 1. **Slice-D reflection** — the cheap in-process name-detection scan
///    (kept as the zero-cost fallback when extraction is disabled or no
///    default model is configured; the gate's duplicate-pending rule
///    dedupes any overlap with the extractor).
/// 2. **B14 post-turn extraction** — one background LLM pass over the
///    finished exchange feeding working memory + the profile-proposal
///    gate + the workspace anchor-proposal gate. Spawned so it never
///    delays (or fails) the turn reply. This is the pass the memory rule
///    has always promised the model exists (B4 — now it does).
/// 3. **M1 auto-summary producer** — when the conversation has crossed
///    another 5-message boundary, regenerate its `auto_summary` +
///    embedding (the tier-2 gather slot and history-search cosine
///    ranking both feed off it). Spawned fail-soft, kill switch
///    `WYLDE_AUTO_SUMMARY=off`.
fn run_post_turn_hooks(
    conversation_id: &str,
    workspace_id: Option<&str>,
    user_message: &str,
    final_message: &str,
) {
    crate::user_profile::reflection::reflect_after_turn(conversation_id, user_message);

    let conv = conversation_id.to_owned();
    let ws = workspace_id.map(str::to_owned);
    let user = user_message.to_owned();
    let assistant = final_message.to_owned();
    tokio::spawn(async move {
        let stats =
            crate::memory::post_turn_extractor::run(&conv, ws.as_deref(), &user, &assistant).await;
        tracing::debug!(?stats, "post-turn extraction finished");
    });

    let conv = conversation_id.to_owned();
    let ws = workspace_id.map(str::to_owned);
    tokio::spawn(async move {
        crate::chat::search::summary::maybe_refresh(&conv, ws.as_deref()).await;
    });
}

/// Driver-side surfacing of the M3 tier-7 degrade flag: a warn log per
/// affected turn (the shrunk slots carry their own in-prompt markers;
/// a Settings/UI annotation is the deferred B8 surface).
fn log_tier7_degrade(
    gathered: &context_gather::GatheredContext,
    conversation_id: &str,
    slot_budget: usize,
) {
    if gathered.tier7_degraded {
        tracing::warn!(
            "turn: tier-7 degrade pass shrank never-drop context for {conversation_id} \
             (slot budget {slot_budget} tokens — small context window)"
        );
    }
}

async fn emit_aborted(
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    reason: AbortReason,
    error: Option<String>,
) {
    handle
        .push_turn_event(TurnEvent::TurnAborted {
            turn_id: turn_id.to_owned(),
            reason,
            error,
        })
        .await;
}

/// Emit the authoritative end-of-turn token meter (chat-processing-indicator).
/// Skipped entirely when both counts are zero (e.g. an Ollama build that
/// omits the eval fields and produced no content) so the GUI never shows a
/// bogus `0` — it just keeps the live tick or hides the meter.
async fn emit_final_usage(
    handle: &Arc<TurnHandle>,
    turn_id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) {
    if prompt_tokens == 0 && completion_tokens == 0 {
        return;
    }
    handle
        .push_turn_event(TurnEvent::Usage {
            turn_id: turn_id.to_owned(),
            prompt_tokens: (prompt_tokens > 0).then_some(prompt_tokens),
            completion_tokens,
            done: true,
        })
        .await;
}

/// Spawn a delayed registry eviction so subscribers that poll just
/// after `mark_done()` still find the handle.
fn schedule_eviction(turn_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        state::remove_turn(&turn_id);
    });
}

async fn emit_unrecognised(handle: &Arc<TurnHandle>, salvage: &SalvageResult) {
    for u in &salvage.unrecognised {
        handle
            .push_tool_event(ToolEvent::ToolError {
                turn_id: handle.turn_id.clone(),
                call_id: u.id.clone(),
                name: u.name.clone(),
                error: format!(
                    "model emitted tool call {:?} in content but the name \
                     doesn't resolve to a known tool",
                    u.name
                ),
                reason: Some(ToolErrorReason::ToolCallTextUnrecognised),
                duration_ms: 0.0,
            })
            .await;
    }
}

/// Build the alias map the salvage parser uses to resolve a
/// model-emitted tool name (dotted, snake-cased, or manifest name) to
/// a canonical tool id.
///
/// Phase 6 populates the map from the in-process tool registry. Every
/// canonical id and every alias key form (dotted, snake, dot/snake
/// inverses) is included so salvage results route into the registry's
/// `lookup` instead of the unrecognised pile.
///
/// MCP-extension namespaces are also seeded so an `extension.tool`
/// name from a model echo isn't dropped into the unrecognised bucket
/// purely on alias-map grounds; the dispatcher then routes via
/// [`crate::dispatch::route`] against `Config::mcp_namespaces`.
fn build_alias_map() -> HashMap<String, String> {
    let mut map = crate::tooling::registry::global().alias_map();
    let cfg = Config::get();
    for ns in &cfg.mcp_namespaces {
        // Seed identity entries for every extension namespace so an
        // emitted call like `webcrawler.scrape` survives salvage as a
        // known token. The dispatcher resolves the actual handler via
        // MCP — alias presence just keeps the call out of
        // `unrecognised`.
        map.entry(ns.clone()).or_insert_with(|| ns.clone());
    }
    map
}

/// Build the opening `messages` array for a chat turn:
/// `[{role:"system", ...tool catalog...}, {role:"user", ...}]`.
///
/// The system message is the Phase-6 prompt port — without it the
/// model is never told tools exist, never emits tool-call JSON, and
/// the salvage parser has nothing to recover. The catalog is read from
/// the in-process registry so the prompt always reflects the live tool
/// set.
/// `workspace_slots` is the active workspace's pre-rendered prompt block
/// (persona / workspace-memory / RAG), fetched from the `wylde-workspaces`
/// service via [`workspace_context::gather`] and appended onto the end of
/// the system prompt. An empty string (no active workspace, or the service
/// degraded) leaves a plain chat turn byte-identical to before.
/// `history` (B1) is the budget-surviving window of prior-turn messages
/// from the gather, spliced between the system message and the current
/// user message so the model finally sees the previous turns.
///
/// Layout (B12, prompt-cache-aware): the system message carries ONLY the
/// stable base prompt (instruction + tool catalog — byte-identical across
/// turns of the same model/mode), and the volatile gathered slots ride at
/// the head of the CURRENT user message, after the history. Ollama's KV
/// prefix reuse therefore covers `system + old history` instead of
/// busting at the first changed slot byte inside the system message —
/// prompt ingestion dominates latency on local hardware. The slots' own
/// `### ` headers plus the explicit `### User message` divider keep the
/// roles unambiguous. Empty slots leave the user message verbatim, so a
/// plain turn is byte-identical to before.
fn initial_messages(
    base_system_prompt: String,
    history: &[Value],
    user_message: &str,
    workspace_slots: &str,
) -> Vec<Value> {
    let slots = workspace_slots.trim();
    let user_content = if slots.is_empty() {
        user_message.to_owned()
    } else {
        format!("{slots}\n\n### User message\n{user_message}")
    };

    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(json!({"role": "system", "content": base_system_prompt}));
    messages.extend(history.iter().cloned());
    messages.push(json!({"role": "user", "content": user_content}));
    messages
}

/// The base system prompt (instruction + live tool catalog) WITHOUT the
/// gathered workspace slots. Built once per turn: it both opens the
/// [`initial_messages`] system message and prices the fixed prompt cost
/// for [`chat_options::slot_budget`] (B5).
///
/// `model` selects the base-instruction variant (B10): native-tool-capable
/// models skip the in-content JSON salvage instruction.
fn base_system_prompt(model: &str) -> String {
    let catalog = crate::tooling::runner::catalog_payload(crate::tooling::registry::global());
    let verb_mode = crate::tooling::resource::verb_mode_active();
    let native = crate::model_registry::heuristics::supports_native_tools(model);
    prompt::build_system_prompt(&catalog, verb_mode, native)
}

/// Build the native Ollama `tools:` request field from the live tool
/// registry. Capable models reply on `message.tool_calls` when this is
/// present; the salvage path stays the fallback. Built from the same
/// catalog the system prompt uses so both advertised tool sets match.
fn tools_payload() -> Vec<Value> {
    let catalog = crate::tooling::runner::catalog_payload(crate::tooling::registry::global());
    prompt::build_tools_field(&catalog, crate::tooling::resource::verb_mode_active())
}

/// Parse a native Ollama `message.tool_calls` array into dispatchable
/// [`ToolCall`]s. Each entry is `{["id"], "function": {"name",
/// "arguments"}}` (Ollama nests under `function`; some builds inline
/// `name`/`arguments`). `arguments` arrives as an object, but a
/// string-encoded object is re-parsed defensively. The emitted name is
/// resolved to its canonical id via `alias_map` so dedupe + summary
/// rows match the salvage path; unknown names pass through verbatim and
/// the dispatcher's registry lookup surfaces the `not_found`. Synthetic
/// ids reset per round (`call_native_<i>`), mirroring the salvage
/// parser's per-round `call_text_<n>` numbering.
fn native_tool_calls(tool_calls: &[Value], alias_map: &HashMap<String, String>) -> Vec<ToolCall> {
    let mut out: Vec<ToolCall> = Vec::new();
    for (i, tc) in tool_calls.iter().enumerate() {
        let func = tc.get("function").unwrap_or(tc);
        let name = match func.get("name").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let raw_args = func.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let args = coerce_native_args(raw_args);
        let canonical = alias_map
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_owned());
        let id = tc
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("call_native_{}", i + 1));
        out.push(ToolCall {
            id,
            name: canonical,
            args,
        });
    }
    out
}

/// Coerce a native `arguments` value into a JSON object. Ollama emits an
/// object; a string-encoded object is re-parsed; any non-object falls
/// back to `{}` (the model gave us nothing usable).
fn coerce_native_args(v: Value) -> Value {
    let v = match v {
        Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Null),
        other => other,
    };
    if v.is_object() {
        v
    } else {
        json!({})
    }
}

/// Pull `message.tool_calls` out of an Ollama reply/chunk as a slice of
/// raw call values, or an empty Vec when absent.
fn extract_native_tool_calls(message_owner: &Value) -> Vec<Value> {
    message_owner
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn recovered_to_calls(salvage: &SalvageResult) -> Vec<ToolCall> {
    salvage
        .recovered
        .iter()
        .cloned()
        .map(RecoveredCall::into)
        .collect()
}

fn tool_calls_wire(calls: &[ToolCall]) -> Vec<Value> {
    calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "function": {"name": c.name, "arguments": c.args},
            })
        })
        .collect()
}

fn string_field(payload: &Value, key: &str) -> Result<String, IpcError> {
    match payload.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(IpcError::new(
            "bad_request",
            format!("{key} is required (non-empty string)"),
        )),
    }
}

fn optional_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn resolve_model(payload: &Value, cfg: &Config) -> String {
    optional_string(payload, "model").unwrap_or_else(|| cfg.default_model.clone())
}

/// Pull `message.content` out of an Ollama `/api/chat` (stream=false)
/// reply for `chat.run_turn`.
fn extract_assistant_content(upstream: &Value) -> String {
    upstream
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Pull the incremental `message.content` piece out of a streaming
/// `ollama.chat_stream` NDJSON chunk.
fn extract_chunk_content(chunk: &Value) -> Option<String> {
    chunk
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Pull a thinking/reasoning delta off an `ollama.chat_stream` chunk
/// (chat-processing-indicator). Thinking-capable models (run with
/// `think: true`) stream their reasoning on `message.thinking` separate from
/// `message.content`; we forward it as [`TurnEvent::Thinking`] so the GUI's
/// activity dropdown can show it. Absent for non-thinking models / configs →
/// `None`, so nothing is emitted (graceful). We do NOT parse `<think>` out of
/// `content` — that would alter the displayed reply.
fn extract_chunk_thinking(chunk: &Value) -> Option<String> {
    chunk
        .get("message")
        .and_then(|m| m.get("thinking"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{get_turn, register_turn};

    // ── 5.A/5.B unary action tests (unchanged) ──────────────────────────

    #[tokio::test]
    async fn run_turn_rejects_missing_user_message() {
        let reply = handle_run_turn(json!({"conversation_id": "c1"})).await;
        assert!(!reply.ok);
        let err = reply.error.unwrap();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("user_message"));
    }

    #[tokio::test]
    async fn run_turn_rejects_missing_conversation_id() {
        let reply = handle_run_turn(json!({"user_message": "hi"})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn run_turn_rejects_empty_strings() {
        let reply = handle_run_turn(json!({
            "user_message": "",
            "conversation_id": "c1",
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[test]
    fn extracts_assistant_content_or_empty_string() {
        let v = json!({"message": {"role": "assistant", "content": "hello"}});
        assert_eq!(extract_assistant_content(&v), "hello");
        let v = json!({"message": {"role": "assistant"}});
        assert_eq!(extract_assistant_content(&v), "");
        let v = json!({});
        assert_eq!(extract_assistant_content(&v), "");
    }

    #[test]
    fn extract_chunk_content_returns_incremental_piece() {
        let v = json!({"message": {"role": "assistant", "content": "Hel"}, "done": false});
        assert_eq!(extract_chunk_content(&v), Some("Hel".to_owned()));
        let v = json!({"done": true});
        assert_eq!(extract_chunk_content(&v), None);
    }

    // ── chat.complete validation (Wylde_Study S2a) ─────────────────────
    // The happy path (real ollama.chat round-trip) is covered by the
    // mock-pipe integration test in tests/chat_complete_e2e.rs; here we
    // pin the cheap arg-validation that needs no backend.

    #[tokio::test]
    async fn complete_rejects_missing_prompt() {
        let reply = handle_complete(json!({})).await;
        assert!(!reply.ok);
        let err = reply.error.unwrap();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("prompt"));
    }

    #[tokio::test]
    async fn complete_rejects_empty_prompt() {
        let reply = handle_complete(json!({"prompt": ""})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn cancel_rejects_missing_turn_id() {
        let reply = handle_cancel(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn cancel_unknown_turn_returns_cancelled_false() {
        let reply = handle_cancel(json!({"turn_id": "no-such-turn-actions-test"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["cancelled"], false);
        assert_eq!(reply.data["turn_id"], "no-such-turn-actions-test");
    }

    #[tokio::test]
    async fn cancel_in_flight_turn_returns_cancelled_true() {
        let id = state::new_turn_id();
        let _ = register_turn(id.clone(), "c1".into());

        let reply = handle_cancel(json!({"turn_id": id.clone()})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["cancelled"], true);
        assert_eq!(reply.data["turn_id"], id);

        let reply = handle_cancel(json!({"turn_id": id.clone()})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["cancelled"], false);

        state::remove_turn(&id);
    }

    #[tokio::test]
    async fn stream_turn_rejects_missing_turn_id() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_stream_turn(json!({}), tx).await;
        let first = rx.recv().await.expect("at least one frame");
        assert!(first.is_err());
        assert_eq!(first.unwrap_err().code, "bad_request");
    }

    #[tokio::test]
    async fn stream_turn_returns_not_found_for_unknown_turn() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_stream_turn(json!({"turn_id": "no-such-turn-stream-test"}), tx).await;
        let first = rx.recv().await.expect("at least one frame");
        assert!(first.is_err());
        assert_eq!(first.unwrap_err().code, "not_found");
    }

    #[tokio::test]
    async fn stream_turn_emits_buffered_events_and_exits_on_done() {
        let id = state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());

        handle
            .push_turn_event(TurnEvent::Token {
                turn_id: id.clone(),
                text: "Hel".into(),
            })
            .await;
        handle
            .push_turn_event(TurnEvent::Token {
                turn_id: id.clone(),
                text: "lo".into(),
            })
            .await;
        handle
            .push_turn_event(TurnEvent::TurnComplete {
                turn_id: id.clone(),
                final_message: "Hello".into(),
            })
            .await;
        handle.mark_done();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        handle_stream_turn(json!({"turn_id": id.clone()}), tx).await;

        let mut got: Vec<Value> = Vec::new();
        while let Some(item) = rx.recv().await {
            got.push(item.expect("ok chunk"));
        }
        assert_eq!(got.len(), 3, "expected 3 buffered events, got {got:?}");
        assert_eq!(got[0]["type"], "token");
        assert_eq!(got[0]["text"], "Hel");
        assert_eq!(got[1]["type"], "token");
        assert_eq!(got[1]["text"], "lo");
        assert_eq!(got[2]["type"], "turn_complete");
        assert_eq!(got[2]["final_message"], "Hello");

        state::remove_turn(&id);
    }

    #[tokio::test]
    async fn stream_tools_can_emit_chunks_in_5c() {
        // 5.C: ToolEvents land on the tool buffer. Manually push one
        // through the handle (the dispatch path that produces them
        // lives in tool_round.rs and is exercised by that module's
        // tests; this test pins the stream surface).
        let id = state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());

        handle
            .push_tool_event(ToolEvent::ToolDispatched {
                turn_id: id.clone(),
                call_id: "c1".into(),
                name: "fs.read".into(),
                args: json!({"path": "foo"}),
            })
            .await;
        handle.mark_done();

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_stream_tools(json!({"turn_id": id.clone()}), tx).await;

        let mut got = Vec::new();
        while let Some(item) = rx.recv().await {
            got.push(item.expect("ok"));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["type"], "tool_dispatched");
        assert_eq!(got[0]["name"], "fs.read");
        state::remove_turn(&id);
    }

    #[tokio::test]
    async fn stream_turn_supports_multiple_subscribers_per_turn() {
        let id = state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());

        handle
            .push_turn_event(TurnEvent::Token {
                turn_id: id.clone(),
                text: "x".into(),
            })
            .await;
        handle.mark_done();

        for _ in 0..2 {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            handle_stream_turn(json!({"turn_id": id.clone()}), tx).await;
            let mut got = Vec::new();
            while let Some(item) = rx.recv().await {
                got.push(item.expect("ok"));
            }
            assert_eq!(got.len(), 1);
            assert_eq!(got[0]["text"], "x");
        }

        state::remove_turn(&id);
    }

    #[tokio::test]
    async fn start_turn_rejects_missing_fields() {
        let reply = handle_start_turn(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn start_turn_returns_turn_id_immediately_when_inputs_valid() {
        // The spawned driving task runs the REAL gather against the
        // process-global WYLDE_DATA_DIR — including the B3 long-term
        // injection, whose touch_all WRITES the store it finds there.
        // Without this sandbox the task brushed whichever other test's
        // tempdir was live (the env lock can't protect against a writer
        // that never takes it), intermittently failing the long-term
        // selective-touch assertions. Holding TestEnv both serializes
        // this test against every store-touching test and points the
        // driver at its own throwaway dir.
        let _env = crate::user_profile::test_support::TestEnv::new();
        let id = state::new_turn_id();
        let reply = handle_start_turn(json!({
            "user_message": "hi",
            "conversation_id": "c1",
            "turn_id": id.clone(),
            "model": "stub-model",
        }))
        .await;

        assert!(reply.ok, "start_turn should accept valid inputs: {reply:?}");
        assert_eq!(reply.data["turn_id"], id);
        assert_eq!(reply.data["conversation_id"], "c1");
        assert!(get_turn(&id).is_some());

        state::cancel_turn(&id);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        state::remove_turn(&id);
    }

    // ── 5.C wire-shape tests ─────────────────────────────────────────────

    #[test]
    fn tool_calls_wire_shape_matches_ollama_function_form() {
        let calls = vec![ToolCall {
            id: "c1".into(),
            name: "fs.read".into(),
            args: json!({"path": "foo"}),
        }];
        let wire = tool_calls_wire(&calls);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["id"], "c1");
        assert_eq!(wire[0]["function"]["name"], "fs.read");
        assert_eq!(wire[0]["function"]["arguments"]["path"], "foo");
    }

    #[test]
    fn recovered_to_calls_preserves_order_and_args() {
        let salvage = SalvageResult {
            cleaned_text: String::new(),
            recovered: vec![
                RecoveredCall {
                    id: "call_text_1".into(),
                    name: "fs.read".into(),
                    args: json!({"a": 1}),
                    raw_name: "fs.read".into(),
                },
                RecoveredCall {
                    id: "call_text_2".into(),
                    name: "git.status".into(),
                    args: json!({}),
                    raw_name: "git.status".into(),
                },
            ],
            unrecognised: Vec::new(),
        };
        let calls = recovered_to_calls(&salvage);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_text_1");
        assert_eq!(calls[1].name, "git.status");
    }

    // ── Phase 6 system-prompt injection tests ───────────────────────────

    #[tokio::test]
    async fn initial_messages_prepends_system_tool_catalog() {
        // The opening messages array must be [system, user], and the
        // system content must advertise tools (a known tool name from
        // the live registry) so the model emits tool-call JSON instead
        // of claiming it has no tools.
        let messages = initial_messages(
            base_system_prompt("stub-model"),
            &[],
            "What time is it?",
            "",
        );
        assert_eq!(messages.len(), 2, "expected [system, user]: {messages:?}");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "What time is it?");

        let system = messages[0]["content"]
            .as_str()
            .expect("system content is a string");
        assert!(
            system.contains("Available tools:"),
            "system prompt must list tools: {system}"
        );
        // Post-Slice-6 cutover (verb mode default on): the verb tools are
        // the always-on surface, and resource-backed named tools like
        // `fs.read_file` are retired from advertising.
        assert!(
            system.contains("wylde_search"),
            "system prompt must advertise the verb tools: {system}"
        );
        assert!(
            !system.contains("fs.read_file"),
            "resource-backed named tools must be retired in verb mode: {system}"
        );
    }

    #[tokio::test]
    async fn initial_messages_splices_history_between_system_and_user() {
        // B1: prior turns ride between the system message and the current
        // user message, chronological.
        let history = vec![
            json!({"role": "user", "content": "earlier question"}),
            json!({"role": "assistant", "content": "earlier answer"}),
        ];
        let messages =
            initial_messages(base_system_prompt("stub-model"), &history, "follow-up", "");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "earlier question");
        assert_eq!(messages[2]["content"], "earlier answer");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "follow-up");
    }

    #[tokio::test]
    async fn gathered_slots_ride_the_user_message_not_the_system_prompt() {
        // B12: the system message stays byte-stable across turns; volatile
        // gathered slots ride at the head of the current user message.
        let slots = "\n\n### User profile\nName: Sam";
        let with_slots = initial_messages(base_system_prompt("stub-model"), &[], "hello", slots);
        let plain = initial_messages(base_system_prompt("stub-model"), &[], "hello", "");

        assert_eq!(
            with_slots[0]["content"], plain[0]["content"],
            "system message identical with and without slots"
        );
        let user = with_slots[1]["content"].as_str().unwrap();
        assert!(user.starts_with("### User profile"), "slots lead: {user}");
        assert!(user.ends_with("### User message\nhello"), "{user}");
        // A plain turn's user message is verbatim.
        assert_eq!(plain[1]["content"], "hello");
    }

    #[tokio::test]
    async fn native_capable_model_gets_the_lean_base_instruction() {
        // B10: a native-tools family skips the in-content JSON instruction.
        let native = base_system_prompt("qwen2.5:7b");
        assert!(
            !native.contains("respond with a single JSON object"),
            "no salvage instruction for native-capable models"
        );
        assert!(native.contains("You are Wylde"));
        assert!(native.contains("Available tools:"));

        let salvage = base_system_prompt("totally-custom-model");
        assert!(
            salvage.contains("respond with a single JSON object"),
            "unknown models keep the salvage instruction"
        );
    }

    #[tokio::test]
    async fn run_turn_request_body_carries_system_then_user() {
        // Mirror the exact body construction in handle_run_turn /
        // drive_streaming_turn: messages = initial_messages(...), then
        // folded into the Ollama request body. Assert the wire shape.
        let messages = initial_messages(
            base_system_prompt("stub-model"),
            &[],
            "Read the README and tell me the license",
            "",
        );
        let body = json!({
            "model": "stub-model",
            "messages": messages,
            "stream": false,
        });
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        // Verb-mode cutover: the verb tools are advertised, not the
        // retired resource-backed named tools.
        assert!(msgs[0]["content"]
            .as_str()
            .unwrap()
            .contains("wylde_search"));
    }

    // ── Fix B: native Ollama `tools:` field + tool_calls parsing ─────────

    #[tokio::test]
    async fn request_body_carries_tools_field_with_json_schema() {
        // Mirror the body construction in handle_run_turn /
        // drive_streaming_turn: tools = tools_payload(), folded into the
        // request body. Assert the wire shape Ollama expects.
        let tools = tools_payload();
        let body = json!({
            "model": "stub-model",
            "messages": initial_messages(base_system_prompt("stub-model"), &[], "hi", ""),
            "tools": tools,
            "stream": false,
        });
        let arr = body["tools"].as_array().expect("tools array present");
        assert!(!arr.is_empty(), "at least one active tool advertised");

        // Every entry is an OpenAI-style function spec.
        for t in arr {
            assert_eq!(t["type"], "function");
            assert!(t["function"]["name"].as_str().is_some());
            assert_eq!(t["function"]["parameters"]["type"], "object");
        }

        // Verb-mode cutover: `fs.read_file` is retired; the verb tool
        // `wylde_get` is the always-on equivalent. Check its required
        // `resource_type` arg surfaces as a JSON-schema string property.
        let wylde_get = arr
            .iter()
            .find(|t| t["function"]["name"] == "wylde_get")
            .expect("wylde_get advertised in tools field");
        let params = &wylde_get["function"]["parameters"];
        assert_eq!(params["properties"]["resource_type"]["type"], "string");
        assert!(params["required"]
            .as_array()
            .unwrap()
            .contains(&json!("resource_type")));
    }

    #[test]
    fn native_tool_calls_parses_ollama_function_shape() {
        let alias_map = build_alias_map();
        let raw = vec![json!({
            "function": {"name": "fs.read_file", "arguments": {"path": "README.md"}},
        })];
        let calls = native_tool_calls(&raw, &alias_map);
        assert_eq!(calls.len(), 1);
        // Name resolves to canonical id via the alias map.
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].args, json!({"path": "README.md"}));
        assert_eq!(calls[0].id, "call_native_1");
    }

    #[test]
    fn native_tool_calls_reparses_string_arguments_and_skips_nameless() {
        let alias_map = build_alias_map();
        let raw = vec![
            json!({"function": {"name": "fs.read_file", "arguments": "{\"path\": \"x\"}"}}),
            json!({"function": {"arguments": {"path": "y"}}}), // no name → skipped
        ];
        let calls = native_tool_calls(&raw, &alias_map);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args, json!({"path": "x"}));
    }

    #[test]
    fn extract_native_tool_calls_returns_empty_when_absent() {
        let v = json!({"message": {"role": "assistant", "content": "hi"}});
        assert!(extract_native_tool_calls(&v).is_empty());
        let v = json!({"message": {"tool_calls": [{"function": {"name": "x"}}]}});
        assert_eq!(extract_native_tool_calls(&v).len(), 1);
    }

    #[tokio::test]
    async fn emit_unrecognised_fires_one_tool_error_per_entry() {
        let id = state::new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());
        let salvage = SalvageResult {
            cleaned_text: String::new(),
            recovered: Vec::new(),
            unrecognised: vec![crate::turn::salvage::UnrecognisedCall {
                id: "call_text_1".into(),
                name: "nonexistent".into(),
                args: json!({}),
            }],
        };
        emit_unrecognised(&handle, &salvage).await;
        let buf = handle.tool_events.lock().await;
        assert_eq!(buf.len(), 1);
        assert!(matches!(
            buf[0],
            ToolEvent::ToolError {
                reason: Some(ToolErrorReason::ToolCallTextUnrecognised),
                ..
            }
        ));
        drop(buf);
        state::remove_turn(&id);
    }
}
