//! Phase 12.1 in-process trait surface.
//!
//! The harness exposes its action surface as the [`HarnessApi`] trait so
//! callers in the same process (the Tauri GUI, primarily — see
//! `Core/GUI/src-tauri/src/pipe/`) can dispatch verbs without round-tripping
//! through the named-pipe IPC. The harness binary's own IPC pipe server
//! still serves the same verbs by registering thin closures over the same
//! trait (see [`crate::pipe::install_all_against`]); external clients (MCP,
//! CLI) keep their pipe access unchanged.
//!
//! ## Why a trait instead of free functions
//!
//! Two reasons:
//!
//! 1. **Caller injection.** The Tauri side and the harness binary both
//!    instantiate an `impl HarnessApi` once and pass it down to their
//!    dispatchers. Tests can substitute a mock impl that records calls.
//! 2. **Per-verb statics.** Compile-time enforcement that every IPC verb in
//!    [`crate::pipe::ALL_PIPE_ACTIONS`] has a corresponding method. Removing
//!    a method without removing the verb (or vice versa) is a build error
//!    on the harness side because the pipe registration loop needs both.
//!
//! ## Wire shape
//!
//! Every method takes a [`serde_json::Value`] payload and returns a
//! [`wylde_shared::ipc::Reply`] — the same envelope the over-the-wire IPC
//! path returns. Streaming methods additionally take a
//! [`wylde_shared::ipc::StreamSender`] and have no return value (chunks go
//! out via the sender; end-of-stream is signalled by dropping it). This
//! means the Tauri-side dispatcher can splice a trait call into the same
//! result shape its frontend already expects from the previous pipe path.
//!
//! ## JSON shaping ownership
//!
//! Pre-Phase-12.1, `pipe/tools.rs` and `pipe/memory_long_term.rs` carried
//! the JSON payload validation + reply-envelope reshaping for their verbs.
//! Phase 12.1 moves that logic into this file's [`DefaultHarnessApi`]
//! methods so both the harness binary's pipe registration and the
//! Tauri-side in-process dispatcher share one implementation. The
//! `pipe/chat.rs` and `pipe/memory_workspaces.rs` files were thin
//! registration shells over verb handlers that already live in
//! [`crate::turn::actions`] and the retired `memory.workspaces` actions —
//! those trait methods are pass-throughs.
//!
//! ## File layout (architecture-review R1)
//!
//! Split from the single-file `api.rs` per architecture-review R1: the
//! consent machinery lives in `consent`, the shared JSON helpers in
//! `helpers`, and the per-domain unit tests in the `tests_*` modules.
//! The trait and the single `impl HarnessApi for DefaultHarnessApi`
//! block stay here -- a trait impl cannot be split across files.

use async_trait::async_trait;
use serde_json::{json, Value};
use wylde_shared::ipc::{Reply, StreamSender};

use crate::chat::search::api as chat_search;
use crate::config::Config;
use crate::memory::conversations::actions as conversations_actions;
use crate::memory::long_term::{self, SaveError};
use crate::memory::short_term::actions as short_term_actions;
use crate::memory::workspace::actions as workspace_memory_actions;
use crate::model_registry::actions as model_actions;
use crate::settings::actions as settings_actions;
use crate::tooling::registry::global;
use crate::tooling::runner::{catalog_payload, dispatch_tool};
use crate::turn::actions as turn_actions;
use crate::turn::tool_round::TIER_TOOL_USE;
use crate::user_profile::api as user_profile_actions;

mod consent;
mod helpers;

#[cfg(test)]
mod tests_memory;
#[cfg(test)]
mod tests_tools;

use consent::{consent_snapshot_value, consent_stream_pending_impl, handle_consent_decide};
use helpers::{float_array, record_to_value, string_array};
pub(crate) use helpers::{optional_string, require_string};

/// The harness's GUI-facing action surface, expressed as Rust methods so
/// in-process callers (Tauri) can dispatch without the IPC hop.
///
/// One method per verb in [`crate::pipe::ALL_PIPE_ACTIONS`]; method names
/// mirror the verb names with `.` swapped for `_`. Streaming verbs have
/// no return; unary verbs return [`Reply`].
#[async_trait]
pub trait HarnessApi: Send + Sync {
    // ── chat.* (7 verbs) ─────────────────────────────────────────────
    async fn chat_run_turn(&self, payload: Value) -> Reply;
    /// `chat.preview_context` — concept-routing R2 Phase 1: route + return the
    /// curate-before-inject candidate menu (no injection, no LLM).
    async fn chat_preview_context(&self, payload: Value) -> Reply;
    async fn chat_complete(&self, payload: Value) -> Reply;
    async fn chat_start_turn(&self, payload: Value) -> Reply;
    async fn chat_cancel(&self, payload: Value) -> Reply;
    async fn chat_stream_turn(&self, payload: Value, sender: StreamSender);
    async fn chat_stream_tools(&self, payload: Value, sender: StreamSender);

    // ── chat.* history search (3 verbs; Thought Bubble System Slice E) ─
    // Scoped chat-history recall. Strict workspace boundary enforced in
    // `chat::search::scope`. In-process dispatch; the workspace backend is
    // reached over the pipe via wylde-workspaces-client.
    async fn chat_search_history(&self, payload: Value) -> Reply;
    async fn chat_list_recent(&self, payload: Value) -> Reply;
    async fn chat_get_conversation(&self, payload: Value) -> Reply;

    // ── chat.* export / import (2 verbs; TBS Slice J) ────────────────
    // The escape hatch: standalone served in-process, workspace forwarded
    // to the workspaces service. Dispatch lives in `chat::exchange`.
    async fn chat_export(&self, payload: Value) -> Reply;
    async fn chat_import(&self, payload: Value) -> Reply;

    // ── tools.* (2 verbs) ────────────────────────────────────────────
    async fn tools_list(&self, payload: Value) -> Reply;
    async fn tools_run(&self, payload: Value) -> Reply;

    // ── models.* (8 verbs; harness Slice 3a) ─────────────────────────
    // `models.transcribe` / `models.synthesize` were retired at the voice
    // cutover and deleted in the Bucket-A IPC cleanup (STT/TTS run
    // in-process in `wylde-voice`, reached via `voice.*`).
    async fn models_list(&self, payload: Value) -> Reply;
    async fn models_get_profile(&self, payload: Value) -> Reply;
    async fn models_show(&self, payload: Value) -> Reply;
    async fn models_delete(&self, payload: Value) -> Reply;
    async fn models_unload(&self, payload: Value) -> Reply;
    async fn models_set_active(&self, payload: Value) -> Reply;
    async fn models_set_default(&self, payload: Value) -> Reply;
    async fn models_get_default(&self, payload: Value) -> Reply;
    async fn models_get_effective(&self, payload: Value) -> Reply;

    // ── settings.ollama.* (4 verbs; per-model inference override store) ─
    async fn settings_ollama_get_overrides(&self, payload: Value) -> Reply;
    async fn settings_ollama_set_overrides(&self, payload: Value) -> Reply;
    async fn settings_ollama_clear_override(&self, payload: Value) -> Reply;
    async fn settings_ollama_list_models_with_overrides(&self, payload: Value) -> Reply;

    // ── settings.encryption.* (encryption-at-rest toggle, OI-14) ─────────
    async fn settings_encryption_get(&self, payload: Value) -> Reply;
    async fn settings_encryption_set(&self, payload: Value) -> Reply;

    // ── settings.concept_routing.* (routing master toggle, concept-routing
    //    plan §3) ───────────────────────────────────────────────────────
    async fn settings_concept_routing_get(&self, payload: Value) -> Reply;
    async fn settings_concept_routing_set(&self, payload: Value) -> Reply;

    // ── settings.reasoning.* + reasoning.fit_check (agentic-reasoning S1:
    //    master toggle + model slots + advisory VRAM fit) ───────────────
    async fn settings_reasoning_get(&self, payload: Value) -> Reply;
    async fn settings_reasoning_set(&self, payload: Value) -> Reply;
    async fn reasoning_fit_check(&self, payload: Value) -> Reply;

    // prompts.* (5 verbs; system-prompt overrides + presets) — Rust
    // port of the Python `_prompts.py` actions (full-Rust cutover).
    async fn prompts_list(&self, payload: Value) -> Reply;
    async fn prompts_save(&self, payload: Value) -> Reply;
    async fn prompts_save_preset(&self, payload: Value) -> Reply;
    async fn prompts_set_active(&self, payload: Value) -> Reply;
    async fn prompts_delete_preset(&self, payload: Value) -> Reply;

    // ── memory.long_term.* (6 verbs) ─────────────────────────────────
    async fn memory_long_term_list(&self, payload: Value) -> Reply;
    async fn memory_long_term_save(&self, payload: Value) -> Reply;
    async fn memory_long_term_update(&self, payload: Value) -> Reply;
    async fn memory_long_term_delete(&self, payload: Value) -> Reply;
    async fn memory_long_term_history(&self, payload: Value) -> Reply;
    async fn memory_long_term_search(&self, payload: Value) -> Reply;

    // ── memory.workspace.* (6 verbs; full-Rust cutover R2a) ──────────
    async fn memory_workspace_list(&self, payload: Value) -> Reply;
    async fn memory_workspace_search(&self, payload: Value) -> Reply;
    async fn memory_workspace_save(&self, payload: Value) -> Reply;
    async fn memory_workspace_update(&self, payload: Value) -> Reply;
    async fn memory_workspace_delete(&self, payload: Value) -> Reply;
    async fn memory_workspace_curate(&self, payload: Value) -> Reply;

    // ── memory.reflect (1 verb; full-Rust cutover R2b) ───────────────
    async fn memory_reflect(&self, payload: Value) -> Reply;

    // ── workspaces.* — RETIRED from the harness (Slice 0d) ───────────
    // All workspace verbs moved to the wylde-workspaces service; the
    // harness is a pure client. No HarnessApi methods remain.

    // ── memory.short_term.* (3 verbs; conversation working memory) ───
    async fn memory_short_term_get(&self, payload: Value) -> Reply;
    async fn memory_short_term_append(&self, payload: Value) -> Reply;
    async fn memory_short_term_clear(&self, payload: Value) -> Reply;

    // ── conversations.* (9 verbs; lifecycle + active sel + workspace) ─
    async fn conversations_new(&self, payload: Value) -> Reply;
    async fn conversations_list(&self, payload: Value) -> Reply;
    async fn conversations_get(&self, payload: Value) -> Reply;
    async fn conversations_delete(&self, payload: Value) -> Reply;
    async fn conversations_delete_by_workspace(&self, payload: Value) -> Reply;
    async fn conversations_get_active(&self, payload: Value) -> Reply;
    async fn conversations_set_active(&self, payload: Value) -> Reply;
    async fn conversations_get_active_for_workspace(&self, payload: Value) -> Reply;
    async fn conversations_set_active_for_workspace(&self, payload: Value) -> Reply;
    async fn conversations_set_workspace(&self, payload: Value) -> Reply;

    // ── consent.* (6 verbs + 1 streaming; Phase 12.2 + 12.6) ─────────
    async fn consent_list(&self, payload: Value) -> Reply;
    async fn consent_set(&self, payload: Value) -> Reply;
    async fn consent_respond(&self, payload: Value) -> Reply;
    async fn consent_clear(&self, payload: Value) -> Reply;
    async fn consent_set_no_auth(&self, payload: Value) -> Reply;
    async fn consent_reset(&self, payload: Value) -> Reply;
    async fn consent_stream_pending(&self, payload: Value, sender: StreamSender);

    // ── user_profile.* (in-process; Thought Bubble System Slice D) ───
    async fn user_profile_get(&self, payload: Value) -> Reply;
    async fn user_profile_update(&self, payload: Value) -> Reply;
    async fn user_profile_propose(&self, payload: Value) -> Reply;
    async fn user_profile_accept(&self, payload: Value) -> Reply;
    async fn user_profile_reject(&self, payload: Value) -> Reply;
    async fn user_profile_list_proposals(&self, payload: Value) -> Reply;
}

/// In-process implementation that delegates to the harness's own
/// subsystems. Stateless — a single instance is shared by every caller in
/// the process; the subsystems hold their state in their own statics
/// (turn registry, long-term store, workspaces registry).
#[derive(Default, Clone, Copy)]
pub struct DefaultHarnessApi;

#[async_trait]
impl HarnessApi for DefaultHarnessApi {
    // ── chat.* ───────────────────────────────────────────────────────
    // Thin pass-throughs. The actual driver lives in turn::actions.

    async fn chat_run_turn(&self, payload: Value) -> Reply {
        turn_actions::handle_run_turn(payload).await
    }

    async fn chat_preview_context(&self, payload: Value) -> Reply {
        turn_actions::handle_preview_context(payload).await
    }

    async fn chat_complete(&self, payload: Value) -> Reply {
        turn_actions::handle_complete(payload).await
    }

    async fn chat_start_turn(&self, payload: Value) -> Reply {
        turn_actions::handle_start_turn(payload).await
    }

    async fn chat_cancel(&self, payload: Value) -> Reply {
        turn_actions::handle_cancel(payload).await
    }

    async fn chat_stream_turn(&self, payload: Value, sender: StreamSender) {
        turn_actions::handle_stream_turn(payload, sender).await;
    }

    async fn chat_stream_tools(&self, payload: Value, sender: StreamSender) {
        turn_actions::handle_stream_tools(payload, sender).await;
    }

    // ── chat.* history search (Slice E) ──────────────────────────────
    // Pass-throughs — scope resolution + ranking live in chat::search.

    async fn chat_search_history(&self, payload: Value) -> Reply {
        chat_search::handle_search_history(payload).await
    }

    async fn chat_list_recent(&self, payload: Value) -> Reply {
        chat_search::handle_list_recent(payload).await
    }

    async fn chat_get_conversation(&self, payload: Value) -> Reply {
        chat_search::handle_get_conversation(payload).await
    }

    // ── chat.* export / import (Slice J) ─────────────────────────────

    async fn chat_export(&self, payload: Value) -> Reply {
        crate::chat::exchange::handle_export(payload).await
    }

    async fn chat_import(&self, payload: Value) -> Reply {
        crate::chat::exchange::handle_import(payload).await
    }

    // ── tools.* ──────────────────────────────────────────────────────
    // JSON shaping lives here (moved from pipe/tools.rs).

    async fn tools_list(&self, _payload: Value) -> Reply {
        let catalog = catalog_payload(global());
        let count = catalog.len();
        Reply::ok(json!({ "tools": catalog, "count": count }))
    }

    async fn tools_run(&self, payload: Value) -> Reply {
        let Some(name) = require_string(&payload, "name") else {
            return Reply::err_msg("bad_request", "name is required");
        };

        let args = match payload.get("args") {
            None | Some(Value::Null) => Value::Object(Default::default()),
            Some(v) if v.is_object() => v.clone(),
            Some(_) => return Reply::err_msg("bad_request", "args must be a map"),
        };

        let device_tier = payload
            .get("device_tier")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(TIER_TOOL_USE);

        let cfg = Config::get();
        let outcome = dispatch_tool(global(), cfg, &name, device_tier, args).await;

        match outcome.result {
            Ok(data) => Reply::ok(json!({
                "ok": true,
                "data": data,
                "canonical_id": outcome.canonical_id,
                "elapsed_ms": outcome.elapsed_ms,
            })),
            Err(dispatch_err) => Reply::ok(json!({
                "ok": false,
                "error": {
                    "code": dispatch_err.error.code,
                    "message": dispatch_err.error.message,
                },
                "canonical_id": outcome.canonical_id,
                "elapsed_ms": outcome.elapsed_ms,
            })),
        }
    }

    // ── models.* (harness Slice 3a) ──────────────────────────────────
    // Pass-throughs to model_registry::actions, which own the JSON
    // shaping + the WYLDE_HARNESS_MODELS_IMPL flag gate. The three
    // Ollama-side verbs inject a LiveOllama bound to the configured
    // wylde-ollama service so the handlers stay unit-testable with a
    // fake.

    async fn models_list(&self, payload: Value) -> Reply {
        model_actions::handle_list(payload).await
    }

    async fn models_get_profile(&self, payload: Value) -> Reply {
        model_actions::handle_get_profile(payload).await
    }

    async fn models_show(&self, payload: Value) -> Reply {
        let ollama = model_actions::LiveOllama {
            service: Config::get().ollama_service.clone(),
        };
        model_actions::handle_show(payload, &ollama).await
    }

    async fn models_delete(&self, payload: Value) -> Reply {
        let ollama = model_actions::LiveOllama {
            service: Config::get().ollama_service.clone(),
        };
        model_actions::handle_delete(payload, &ollama).await
    }

    async fn models_unload(&self, payload: Value) -> Reply {
        let ollama = model_actions::LiveOllama {
            service: Config::get().ollama_service.clone(),
        };
        model_actions::handle_unload(payload, &ollama).await
    }

    async fn models_set_active(&self, payload: Value) -> Reply {
        model_actions::handle_set_active(payload).await
    }

    async fn models_set_default(&self, payload: Value) -> Reply {
        model_actions::handle_set_default(payload).await
    }

    async fn models_get_default(&self, payload: Value) -> Reply {
        model_actions::handle_get_default(payload).await
    }

    async fn models_get_effective(&self, payload: Value) -> Reply {
        model_actions::handle_get_effective(payload).await
    }

    // ── settings.ollama.* ────────────────────────────────────────────
    // Pass-throughs to the per-model override store's action handlers.

    async fn settings_ollama_get_overrides(&self, payload: Value) -> Reply {
        settings_actions::handle_get_overrides(payload).await
    }

    async fn settings_ollama_set_overrides(&self, payload: Value) -> Reply {
        settings_actions::handle_set_overrides(payload).await
    }

    async fn settings_ollama_clear_override(&self, payload: Value) -> Reply {
        settings_actions::handle_clear_override(payload).await
    }

    async fn settings_ollama_list_models_with_overrides(&self, payload: Value) -> Reply {
        settings_actions::handle_list_models_with_overrides(payload).await
    }

    // ── settings.encryption.* (OI-14 toggle) ─────────────────────────
    async fn settings_encryption_get(&self, payload: Value) -> Reply {
        settings_actions::handle_encryption_get(payload).await
    }

    async fn settings_encryption_set(&self, payload: Value) -> Reply {
        settings_actions::handle_encryption_set(payload).await
    }

    // ── settings.concept_routing.* (routing master toggle) ────────────
    async fn settings_concept_routing_get(&self, payload: Value) -> Reply {
        settings_actions::handle_concept_routing_get(payload).await
    }

    async fn settings_concept_routing_set(&self, payload: Value) -> Reply {
        settings_actions::handle_concept_routing_set(payload).await
    }

    // ── settings.reasoning.* + reasoning.fit_check (agentic-reasoning S1) ─
    async fn settings_reasoning_get(&self, payload: Value) -> Reply {
        settings_actions::handle_reasoning_get(payload).await
    }

    async fn settings_reasoning_set(&self, payload: Value) -> Reply {
        settings_actions::handle_reasoning_set(payload).await
    }

    async fn reasoning_fit_check(&self, payload: Value) -> Reply {
        crate::turn::reasoning::handle_fit_check(payload).await
    }

    // prompts.* (system-prompt overrides + presets). Synchronous store
    // work; the JSON shaping lives in crate::prompts.

    async fn prompts_list(&self, payload: Value) -> Reply {
        crate::prompts::handle_list(payload)
    }

    async fn prompts_save(&self, payload: Value) -> Reply {
        crate::prompts::handle_save(payload)
    }

    async fn prompts_save_preset(&self, payload: Value) -> Reply {
        crate::prompts::handle_save_preset(payload)
    }

    async fn prompts_set_active(&self, payload: Value) -> Reply {
        crate::prompts::handle_set_active(payload)
    }

    async fn prompts_delete_preset(&self, payload: Value) -> Reply {
        crate::prompts::handle_delete_preset(payload)
    }

    // ── memory.long_term.* ───────────────────────────────────────────
    // JSON shaping lives here (moved from pipe/memory_long_term.rs).

    async fn memory_long_term_list(&self, payload: Value) -> Reply {
        let include = payload
            .get("include_superseded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let records: Vec<Value> = long_term::list_records(include)
            .into_iter()
            .map(record_to_value)
            .collect();
        let count = records.len();
        Reply::ok(json!({ "memories": records, "count": count }))
    }

    async fn memory_long_term_save(&self, payload: Value) -> Reply {
        let Some(body) = require_string(&payload, "body") else {
            return Reply::err_msg("bad_request", "body is required");
        };
        let source =
            optional_string(&payload, "source").unwrap_or_else(|| "settings_ui".to_owned());
        let importance = payload.get("importance").and_then(Value::as_f64);
        let tags = string_array(&payload, "tags");
        // Auto-embed on write (fail-soft, budgeted) so memories saved
        // through this API/pipe path (Settings UI, extensions, N8N —
        // anything that isn't the model tool) still populate
        // `long_term.vec.bin` and stay reachable by semantic search. A
        // caller-supplied `vector` wins; otherwise embed the body — mirrors
        // the model-tool handler (`tooling::tools::memory::run_save`) and
        // the workspace save path. An absent embedder just leaves it to
        // text search. (fix #43)
        let vector = match float_array(&payload, "vector") {
            Some(v) => Some(v),
            None => crate::memory::embed_write::embed_for_write(&body).await,
        };

        match long_term::save(&body, &source, importance, tags, vector) {
            Ok(record) => Reply::ok(record_to_value(record)),
            Err(SaveError::EmptyBody) => Reply::err_msg("bad_request", "body is required"),
            Err(SaveError::Io(e)) => Reply::err_msg("io_error", e.to_string()),
        }
    }

    async fn memory_long_term_update(&self, payload: Value) -> Reply {
        let Some(rid) = require_string(&payload, "id") else {
            return Reply::err_msg("bad_request", "id is required");
        };
        let body = payload
            .get("body")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let source = payload
            .get("source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let importance = payload.get("importance").and_then(Value::as_f64);
        // Re-embed the replacement record so its `long_term.vec.bin` mirror
        // tracks the current text (update mints a NEW record id). Caller
        // `vector` wins; else embed the effective new body — the supplied
        // `body`, or the original's when unchanged. Mirrors the model-tool
        // handler; fail-soft. (fix #43)
        let vector = match float_array(&payload, "vector") {
            Some(v) => Some(v),
            None => {
                let effective_body = body
                    .map(str::to_owned)
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| long_term::get(&rid).map(|r| r.body));
                match effective_body {
                    Some(text) => crate::memory::embed_write::embed_for_write(&text).await,
                    None => None,
                }
            }
        };

        match long_term::update(&rid, body, importance, source, vector) {
            Some(record) => Reply::ok(record_to_value(record)),
            None => Reply::err_msg("not_found", format!("memory {rid:?} not found")),
        }
    }

    async fn memory_long_term_delete(&self, payload: Value) -> Reply {
        let Some(rid) = require_string(&payload, "id") else {
            return Reply::err_msg("bad_request", "id is required");
        };
        let ok = long_term::delete(&rid);
        Reply::ok(json!({ "ok": ok, "id": rid }))
    }

    async fn memory_long_term_history(&self, payload: Value) -> Reply {
        let Some(rid) = require_string(&payload, "id") else {
            return Reply::err_msg("bad_request", "id is required");
        };
        let chain: Vec<Value> = long_term::history(&rid)
            .into_iter()
            .map(record_to_value)
            .collect();
        Reply::ok(json!({ "id": rid, "chain": chain }))
    }

    async fn memory_long_term_search(&self, payload: Value) -> Reply {
        let Some(query) = require_string(&payload, "query") else {
            return Reply::err_msg("bad_request", "query is required");
        };
        let limit = payload.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
        let decay = payload.get("decay_days").and_then(Value::as_f64);
        match long_term::text_search(&query, limit, decay).await {
            Ok(hits) => Reply::ok(json!({
                "results": hits.iter().map(|h| h.to_value()).collect::<Vec<_>>(),
                "count": hits.len(),
            })),
            Err(long_term::TextSearchError::EmptyQuery) => {
                Reply::err_msg("bad_request", "query is empty after trim")
            }
            Err(long_term::TextSearchError::Embed(e)) => {
                Reply::err_msg("embed_failed", e.to_string())
            }
        }
    }

    // ── memory.workspace.* ─────────────────────────────────────────
    // Pass-throughs — JSON shaping lives in memory::workspace::actions
    // (full-Rust cutover slice R2a).

    async fn memory_workspace_list(&self, payload: Value) -> Reply {
        workspace_memory_actions::handle_list(payload).await
    }

    async fn memory_workspace_search(&self, payload: Value) -> Reply {
        workspace_memory_actions::handle_search(payload).await
    }

    async fn memory_workspace_save(&self, payload: Value) -> Reply {
        workspace_memory_actions::handle_save(payload).await
    }

    async fn memory_workspace_update(&self, payload: Value) -> Reply {
        workspace_memory_actions::handle_update(payload).await
    }

    async fn memory_workspace_delete(&self, payload: Value) -> Reply {
        workspace_memory_actions::handle_delete(payload).await
    }

    async fn memory_workspace_curate(&self, payload: Value) -> Reply {
        workspace_memory_actions::handle_curate(payload).await
    }

    // ── memory.reflect ───────────────────────────────────────────────
    // Pass-through — scope dispatch + the production chat wiring live
    // in memory::reflection (full-Rust cutover slice R2b).

    async fn memory_reflect(&self, payload: Value) -> Reply {
        crate::memory::reflection::handle_reflect(payload).await
    }

    // ── workspaces.* — RETIRED from the harness (Slice 0d) ───────────
    // Workspace state lives in the wylde-workspaces service; the harness
    // is a pure client (see crate::turn::workspace_context for the chat
    // driver's gather). No HarnessApi pass-throughs remain.

    // ── memory.short_term.* ──────────────────────────────────────────
    // Pass-throughs — JSON shaping lives in short_term::actions.

    async fn memory_short_term_get(&self, payload: Value) -> Reply {
        short_term_actions::handle_get(payload).await
    }

    async fn memory_short_term_append(&self, payload: Value) -> Reply {
        short_term_actions::handle_append(payload).await
    }

    async fn memory_short_term_clear(&self, payload: Value) -> Reply {
        short_term_actions::handle_clear(payload).await
    }

    // ── conversations.* ──────────────────────────────────────────────
    // Pass-throughs — JSON shaping lives in conversations::actions.

    async fn conversations_new(&self, payload: Value) -> Reply {
        conversations_actions::handle_new(payload).await
    }

    async fn conversations_list(&self, payload: Value) -> Reply {
        conversations_actions::handle_list(payload).await
    }

    async fn conversations_get(&self, payload: Value) -> Reply {
        conversations_actions::handle_get(payload).await
    }

    async fn conversations_delete(&self, payload: Value) -> Reply {
        conversations_actions::handle_delete(payload).await
    }

    async fn conversations_delete_by_workspace(&self, payload: Value) -> Reply {
        conversations_actions::handle_delete_by_workspace(payload).await
    }

    async fn conversations_get_active(&self, payload: Value) -> Reply {
        conversations_actions::handle_get_active(payload).await
    }

    async fn conversations_set_active(&self, payload: Value) -> Reply {
        conversations_actions::handle_set_active(payload).await
    }

    async fn conversations_get_active_for_workspace(&self, payload: Value) -> Reply {
        conversations_actions::handle_get_active_for_workspace(payload).await
    }

    async fn conversations_set_active_for_workspace(&self, payload: Value) -> Reply {
        conversations_actions::handle_set_active_for_workspace(payload).await
    }

    async fn conversations_set_workspace(&self, payload: Value) -> Reply {
        conversations_actions::handle_set_workspace(payload).await
    }

    // ── consent.* (Phase 12.2) ───────────────────────────────────────
    // Per-tool consent gate + global no-auth flag, persisted to
    // <wylde_root>/data/preferences/consent.json. See
    // `crate::tooling::consent` for the store + gate logic.

    async fn consent_list(&self, _payload: Value) -> Reply {
        Reply::ok(consent_snapshot_value())
    }

    async fn consent_set(&self, payload: Value) -> Reply {
        handle_consent_decide(&payload)
    }

    async fn consent_respond(&self, payload: Value) -> Reply {
        // `consent.respond` is the GUI-side response to a pending gate
        // prompt. Semantically identical to `consent.set`; the
        // separate verb name pins it as the response-to-prompt path.
        // Phase 12.6: both honor the optional `remember: false` flag
        // for one-time grants.
        handle_consent_decide(&payload)
    }

    async fn consent_clear(&self, payload: Value) -> Reply {
        let Some(tool_id) = require_string(&payload, "tool_id") else {
            return Reply::err_msg("bad_request", "tool_id is required");
        };
        match crate::tooling::consent::store().clear(&tool_id) {
            Ok(_) => {
                // Dismiss any pending toast for this tool — the user
                // explicitly chose "no decision yet" and the GUI
                // should drop the prompt.
                crate::tooling::consent::resolve_pending_for_tool(&tool_id, None);
                Reply::ok(consent_snapshot_value())
            }
            Err(e) => Reply::err_msg("io_error", e),
        }
    }

    async fn consent_set_no_auth(&self, payload: Value) -> Reply {
        let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
            return Reply::err_msg("bad_request", "enabled (bool) is required");
        };
        match crate::tooling::consent::store().set_no_auth(enabled) {
            Ok(_) => Reply::ok(consent_snapshot_value()),
            Err(e) => Reply::err_msg("io_error", e),
        }
    }

    async fn consent_reset(&self, _payload: Value) -> Reply {
        match crate::tooling::consent::store().reset() {
            Ok(_) => {
                // Reset wipes every decision; the pending toast list
                // is no longer meaningful either.
                crate::tooling::consent::clear_pending();
                Reply::ok(consent_snapshot_value())
            }
            Err(e) => Reply::err_msg("io_error", e),
        }
    }

    async fn consent_stream_pending(&self, payload: Value, sender: StreamSender) {
        consent_stream_pending_impl(payload, sender).await;
    }

    // ── user_profile.* ───────────────────────────────────────────────
    // Pass-throughs — JSON shaping lives in user_profile::api. In-process
    // (no pipe hop, no client tier); see that module's docs.

    async fn user_profile_get(&self, payload: Value) -> Reply {
        user_profile_actions::handle_get(payload).await
    }

    async fn user_profile_update(&self, payload: Value) -> Reply {
        user_profile_actions::handle_update(payload).await
    }

    async fn user_profile_propose(&self, payload: Value) -> Reply {
        user_profile_actions::handle_propose(payload).await
    }

    async fn user_profile_accept(&self, payload: Value) -> Reply {
        user_profile_actions::handle_accept(payload).await
    }

    async fn user_profile_reject(&self, payload: Value) -> Reply {
        user_profile_actions::handle_reject(payload).await
    }

    async fn user_profile_list_proposals(&self, payload: Value) -> Reply {
        user_profile_actions::handle_list_proposals(payload).await
    }
}
