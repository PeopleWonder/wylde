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
use crate::turn::salvage::{self, RecoveredCall, SalvageResult};
use crate::turn::tool_round::{self, ToolCall, ToolRoundState};
use crate::turn::workspace_context;

/// `chat.run_turn` — synchronous chat turn with the 5.C tool-round
/// loop. Drives the LLM → salvage → dispatch cycle up to
/// [`tool_round::MAX_TOOL_LOOPS`] rounds, then returns the final
/// message + a [`tool_round::ToolSummary`] list.
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

    tracing::debug!(
        turn_id = %turn_id,
        conversation_id = %conversation_id,
        model = %model,
        device_tier = %normalised_tier,
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
    let base_prompt = base_system_prompt();
    let slot_budget = chat_options::slot_budget(&model, &base_prompt, &user_message).await;
    let gathered = context_gather::gather(
        workspace_id.as_deref(),
        &user_message,
        &conversation_id,
        &overrides,
        slot_budget,
    )
    .await;
    let mut messages = initial_messages(base_prompt, &user_message, &gathered.system_slots);
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

    // Post-turn reflection (Thought Bubble System Slice D). Scan the just
    // -finished exchange for a candidate user_profile update and surface
    // it (spam-controlled, user-accept) into the pending queue. Best
    // -effort and infallible from here — a refusal or write error is
    // swallowed inside the hook, so reflection can never affect the
    // turn reply. Only runs on a naturally-completed turn (not an abort).
    if completed_naturally {
        crate::user_profile::reflection::reflect_after_turn(&conversation_id, &user_message);
    }

    // Surface the graceful-degradation notice when the workspaces service
    // was requested but unreachable (scope v2 §7.5).
    let final_text = workspace_context::apply_degraded_notice(final_text, gathered.degraded);

    Reply::ok(json!({
        "turn_id": turn_id,
        "conversation_id": conversation_id,
        "final_message": final_text,
        "tool_calls_summary": summary,
        "aborted": aborted_now,
        "abort_reason": abort_reason
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        "abort_error": abort_error.map(Value::String).unwrap_or(Value::Null),
    }))
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

/// Streaming turn driver — the spawned task `chat.start_turn` kicks
/// off. Each round opens a fresh `ollama.chat_stream`, accumulates
/// assistant text silently (Option A), salvages tool calls at
/// stream-complete, dispatches them, and feeds the results back into
/// the next round. Bails out at [`tool_round::MAX_TOOL_LOOPS`] or
/// when there are no more tool calls.
#[allow(clippy::too_many_arguments)] // turn-driver fan-out; grouping into a
                                     // struct would only move the noise. Slice G added `conversation_id`; Slice M
                                     // added the composer's per-message token overrides.
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
) {
    // Gather the turn's context (Slice G) — see `handle_run_turn` for the flow.
    let base_prompt = base_system_prompt();
    let slot_budget = chat_options::slot_budget(&model, &base_prompt, &user_message).await;
    let gathered = context_gather::gather(
        workspace_id.as_deref(),
        &user_message,
        &conversation_id,
        &overrides,
        slot_budget,
    )
    .await;
    let mut messages = initial_messages(base_prompt, &user_message, &gathered.system_slots);
    let tools = tools_payload();
    // Per-model inference overrides ride every round's request (B5).
    let options = chat_options::chat_options(&model);
    let mut round_state = ToolRoundState::new();
    let alias_map = build_alias_map();

    let normalised_tier = tool_round::normalise_device_tier(if device_tier.is_empty() {
        None
    } else {
        Some(device_tier.as_str())
    });

    for round in 0..tool_round::MAX_TOOL_LOOPS {
        round_state.rounds = round + 1;

        if handle.is_cancelled() {
            emit_aborted(&handle, &turn_id, AbortReason::Cancelled, None).await;
            handle.mark_done();
            schedule_eviction(turn_id.clone());
            return;
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
        // Native `message.tool_calls` may arrive on any chunk (Ollama
        // typically emits them whole on the final chunk); accumulate
        // across the stream and decode after it completes.
        let mut native_raw: Vec<Value> = Vec::new();
        let mut errored: Option<String> = None;
        let mut cancelled_mid_round = false;

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
                                accumulated.push_str(&piece);
                            }
                            native_raw.extend(extract_native_tool_calls(&chunk));
                        }
                    }
                }
            }
        }
        drop(stream);

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
            // No tool calls — emit the cleaned text as a single Token
            // event (bulk emit; mirrors Python's no-stream-token path
            // in `_driver.py:416-419`) and finish. Prefix the graceful-
            // degradation notice when the workspaces service was requested
            // but unreachable (scope v2 §7.5).
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

        // Tool calls present — emit any pre-call text (the "let me
        // look that up" mid-stream), then dispatch each call.
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
            messages.push(tool_msg);
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
fn initial_messages(
    base_system_prompt: String,
    user_message: &str,
    workspace_slots: &str,
) -> Vec<Value> {
    let mut system_prompt = base_system_prompt;
    system_prompt.push_str(workspace_slots);

    vec![
        json!({"role": "system", "content": system_prompt}),
        json!({"role": "user", "content": user_message}),
    ]
}

/// The base system prompt (instruction + live tool catalog) WITHOUT the
/// gathered workspace slots. Built once per turn: it both opens the
/// [`initial_messages`] system message and prices the fixed prompt cost
/// for [`chat_options::slot_budget`] (B5).
fn base_system_prompt() -> String {
    let catalog = crate::tooling::runner::catalog_payload(crate::tooling::registry::global());
    let verb_mode = crate::tooling::resource::verb_mode_active();
    prompt::build_system_prompt(&catalog, verb_mode)
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
        let messages = initial_messages(base_system_prompt(), "What time is it?", "");
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
    async fn run_turn_request_body_carries_system_then_user() {
        // Mirror the exact body construction in handle_run_turn /
        // drive_streaming_turn: messages = initial_messages(...), then
        // folded into the Ollama request body. Assert the wire shape.
        let messages = initial_messages(
            base_system_prompt(),
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
            "messages": initial_messages(base_system_prompt(), "hi", ""),
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
