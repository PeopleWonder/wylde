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

use async_trait::async_trait;
use serde_json::{json, Value};
use wylde_shared::ipc::{Reply, StreamSender};

use crate::config::Config;
use crate::memory::conversations::actions as conversations_actions;
use crate::memory::long_term::{self, LongTermMemory, SaveError};
use crate::memory::rag::actions as rag_actions;
use crate::memory::short_term::actions as short_term_actions;
use crate::model_registry::actions as model_actions;
use crate::settings::actions as settings_actions;
use crate::tooling::consent::{self, Decision};
use crate::workspaces::api as workspaces_api;
use crate::workspaces::conversations_api as workspaces_conversations_api;
use crate::workspaces::notes_api as workspaces_notes_api;
use crate::tooling::registry::global;
use crate::tooling::runner::{catalog_payload, dispatch_tool};
use crate::turn::actions as turn_actions;
use crate::turn::tool_round::TIER_TOOL_USE;

/// The harness's GUI-facing action surface, expressed as Rust methods so
/// in-process callers (Tauri) can dispatch without the IPC hop.
///
/// One method per verb in [`crate::pipe::ALL_PIPE_ACTIONS`]; method names
/// mirror the verb names with `.` swapped for `_`. Streaming verbs have
/// no return; unary verbs return [`Reply`].
#[async_trait]
pub trait HarnessApi: Send + Sync {
    // ── chat.* (6 verbs) ─────────────────────────────────────────────
    async fn chat_run_turn(&self, payload: Value) -> Reply;
    async fn chat_complete(&self, payload: Value) -> Reply;
    async fn chat_start_turn(&self, payload: Value) -> Reply;
    async fn chat_cancel(&self, payload: Value) -> Reply;
    async fn chat_stream_turn(&self, payload: Value, sender: StreamSender);
    async fn chat_stream_tools(&self, payload: Value, sender: StreamSender);

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

    // ── rag.* (2 verbs; Wylde_Study S2a) ─────────────────────────────
    async fn rag_add_episodic(&self, payload: Value) -> Reply;
    async fn rag_search(&self, payload: Value) -> Reply;

    // ── memory.long_term.* (6 verbs) ─────────────────────────────────
    async fn memory_long_term_list(&self, payload: Value) -> Reply;
    async fn memory_long_term_save(&self, payload: Value) -> Reply;
    async fn memory_long_term_update(&self, payload: Value) -> Reply;
    async fn memory_long_term_delete(&self, payload: Value) -> Reply;
    async fn memory_long_term_history(&self, payload: Value) -> Reply;
    async fn memory_long_term_search(&self, payload: Value) -> Reply;

    // ── workspaces.* (6 verbs; config-file-backed redesign) ──────────
    async fn workspaces_set_active(&self, payload: Value) -> Reply;
    async fn workspaces_create(&self, payload: Value) -> Reply;
    async fn workspaces_update(&self, payload: Value) -> Reply;
    async fn workspaces_delete(&self, payload: Value) -> Reply;
    async fn workspaces_set_persona(&self, payload: Value) -> Reply;
    async fn workspaces_list_mru(&self, payload: Value) -> Reply;
    async fn workspaces_rag_query(&self, payload: Value) -> Reply;
    async fn workspaces_reindex(&self, payload: Value) -> Reply;

    // ── workspaces.notes.* (6 verbs; Slice 0c — relocated notes tier) ─
    async fn workspaces_notes_list(&self, payload: Value) -> Reply;
    async fn workspaces_notes_add(&self, payload: Value) -> Reply;
    async fn workspaces_notes_update(&self, payload: Value) -> Reply;
    async fn workspaces_notes_delete(&self, payload: Value) -> Reply;
    async fn workspaces_notes_search(&self, payload: Value) -> Reply;
    async fn workspaces_notes_propose(&self, payload: Value) -> Reply;

    // ── workspaces.conversations.* (3 verbs; Slice 0c — workspace convos) ─
    async fn workspaces_conversations_list(&self, payload: Value) -> Reply;
    async fn workspaces_conversations_get(&self, payload: Value) -> Reply;
    async fn workspaces_conversations_delete(&self, payload: Value) -> Reply;

    // ── memory.short_term.* (3 verbs; conversation working memory) ───
    async fn memory_short_term_get(&self, payload: Value) -> Reply;
    async fn memory_short_term_append(&self, payload: Value) -> Reply;
    async fn memory_short_term_clear(&self, payload: Value) -> Reply;

    // ── conversations.* (7 verbs; lifecycle + active sel + workspace) ─
    async fn conversations_new(&self, payload: Value) -> Reply;
    async fn conversations_list(&self, payload: Value) -> Reply;
    async fn conversations_get(&self, payload: Value) -> Reply;
    async fn conversations_delete(&self, payload: Value) -> Reply;
    async fn conversations_get_active(&self, payload: Value) -> Reply;
    async fn conversations_set_active(&self, payload: Value) -> Reply;
    async fn conversations_set_workspace(&self, payload: Value) -> Reply;

    // ── consent.* (6 verbs + 1 streaming; Phase 12.2 + 12.6) ─────────
    async fn consent_list(&self, payload: Value) -> Reply;
    async fn consent_set(&self, payload: Value) -> Reply;
    async fn consent_respond(&self, payload: Value) -> Reply;
    async fn consent_clear(&self, payload: Value) -> Reply;
    async fn consent_set_no_auth(&self, payload: Value) -> Reply;
    async fn consent_reset(&self, payload: Value) -> Reply;
    async fn consent_stream_pending(&self, payload: Value, sender: StreamSender);
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

    // ── rag.* (Wylde_Study S2a) ──────────────────────────────────────
    // Thin pass-throughs to the rag action handlers, which already
    // return the `status`-envelope shape the `rag.*` family uses. These
    // are plain pipe actions (not model-callable tools): like every
    // other action here they delegate straight to the subsystem and do
    // not run the per-tool consent / device-tier gate — that gate is
    // applied by `tools.run`'s dispatcher, not by direct actions.

    async fn rag_add_episodic(&self, payload: Value) -> Reply {
        match rag_actions::run_rag_add_episodic(payload).await {
            Ok(v) => Reply::ok(v),
            Err(e) => Reply::err(e),
        }
    }

    async fn rag_search(&self, payload: Value) -> Reply {
        match rag_actions::run_rag_search(payload).await {
            Ok(v) => Reply::ok(v),
            Err(e) => Reply::err(e),
        }
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

        match long_term::save(&body, &source, importance, tags, None) {
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

        match long_term::update(&rid, body, importance, source, None) {
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
        let limit = payload
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(5) as usize;
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

    // ── workspaces.* ─────────────────────────────────────────────────
    // Pass-throughs — JSON shaping lives in workspaces::api.

    async fn workspaces_set_active(&self, payload: Value) -> Reply {
        workspaces_api::handle_set_active(payload).await
    }

    async fn workspaces_create(&self, payload: Value) -> Reply {
        workspaces_api::handle_create(payload).await
    }

    async fn workspaces_update(&self, payload: Value) -> Reply {
        workspaces_api::handle_update(payload).await
    }

    async fn workspaces_delete(&self, payload: Value) -> Reply {
        workspaces_api::handle_delete(payload).await
    }

    async fn workspaces_set_persona(&self, payload: Value) -> Reply {
        workspaces_api::handle_set_persona(payload).await
    }

    async fn workspaces_list_mru(&self, payload: Value) -> Reply {
        workspaces_api::handle_list_mru(payload).await
    }

    async fn workspaces_rag_query(&self, payload: Value) -> Reply {
        workspaces_api::handle_rag_query(payload).await
    }

    async fn workspaces_reindex(&self, payload: Value) -> Reply {
        workspaces_api::handle_reindex(payload).await
    }

    // ── workspaces.notes.* (Slice 0c) ────────────────────────────────
    async fn workspaces_notes_list(&self, payload: Value) -> Reply {
        workspaces_notes_api::handle_list(payload).await
    }
    async fn workspaces_notes_add(&self, payload: Value) -> Reply {
        workspaces_notes_api::handle_add(payload).await
    }
    async fn workspaces_notes_update(&self, payload: Value) -> Reply {
        workspaces_notes_api::handle_update(payload).await
    }
    async fn workspaces_notes_delete(&self, payload: Value) -> Reply {
        workspaces_notes_api::handle_delete(payload).await
    }
    async fn workspaces_notes_search(&self, payload: Value) -> Reply {
        workspaces_notes_api::handle_search(payload).await
    }
    async fn workspaces_notes_propose(&self, payload: Value) -> Reply {
        workspaces_notes_api::handle_propose(payload).await
    }

    // ── workspaces.conversations.* (Slice 0c) ────────────────────────
    async fn workspaces_conversations_list(&self, payload: Value) -> Reply {
        workspaces_conversations_api::handle_list(payload).await
    }
    async fn workspaces_conversations_get(&self, payload: Value) -> Reply {
        workspaces_conversations_api::handle_get(payload).await
    }
    async fn workspaces_conversations_delete(&self, payload: Value) -> Reply {
        workspaces_conversations_api::handle_delete(payload).await
    }

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

    async fn conversations_get_active(&self, payload: Value) -> Reply {
        conversations_actions::handle_get_active(payload).await
    }

    async fn conversations_set_active(&self, payload: Value) -> Reply {
        conversations_actions::handle_set_active(payload).await
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
        match consent::store().clear(&tool_id) {
            Ok(_) => {
                // Dismiss any pending toast for this tool — the user
                // explicitly chose "no decision yet" and the GUI
                // should drop the prompt.
                consent::resolve_pending_for_tool(&tool_id, None);
                Reply::ok(consent_snapshot_value())
            }
            Err(e) => Reply::err_msg("io_error", e),
        }
    }

    async fn consent_set_no_auth(&self, payload: Value) -> Reply {
        let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
            return Reply::err_msg("bad_request", "enabled (bool) is required");
        };
        match consent::store().set_no_auth(enabled) {
            Ok(_) => Reply::ok(consent_snapshot_value()),
            Err(e) => Reply::err_msg("io_error", e),
        }
    }

    async fn consent_reset(&self, _payload: Value) -> Reply {
        match consent::store().reset() {
            Ok(_) => {
                // Reset wipes every decision; the pending toast list
                // is no longer meaningful either.
                consent::clear_pending();
                Reply::ok(consent_snapshot_value())
            }
            Err(e) => Reply::err_msg("io_error", e),
        }
    }

    async fn consent_stream_pending(&self, payload: Value, sender: StreamSender) {
        consent_stream_pending_impl(payload, sender).await;
    }
}

fn consent_snapshot_value() -> Value {
    let snap = consent::store().snapshot();
    let tools = snap
        .tools
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.as_wire().to_string())))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "no_auth": snap.no_auth,
        "tools": tools,
    })
}

fn parse_consent_set(payload: &Value) -> Result<(String, Decision, bool), String> {
    let Some(tool_id) = require_string(payload, "tool_id") else {
        return Err("tool_id is required".to_owned());
    };
    let decision_str = payload
        .get("decision")
        .and_then(Value::as_str)
        .ok_or_else(|| "decision is required".to_owned())?;
    let decision = match decision_str {
        "approved" => Decision::Approved,
        "denied" => Decision::Denied,
        other => {
            return Err(format!(
                "decision must be \"approved\" or \"denied\"; got {other:?}"
            ))
        }
    };
    // Phase 12.6: `remember: false` makes the decision authorize the
    // current call without writing to disk. Missing → default true to
    // preserve pre-12.6 behaviour for callers that don't know about
    // the flag. A non-bool value is a bad_request — we don't want a
    // string "false" to silently mean "persist".
    let remember = match payload.get("remember") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("remember must be a bool".to_owned()),
    };
    Ok((tool_id, decision, remember))
}

fn handle_consent_decide(payload: &Value) -> Reply {
    match parse_consent_set(payload) {
        Ok((tool_id, decision, remember)) => {
            let resolved_decision = match decision {
                Decision::Approved => "approved",
                Decision::Denied => "denied",
            };
            if remember {
                if let Err(e) = consent::store().set(&tool_id, decision) {
                    return Reply::err_msg("io_error", e);
                }
            } else {
                consent::store().set_one_time(&tool_id, decision);
            }
            consent::resolve_pending_for_tool(&tool_id, Some(resolved_decision));
            Reply::ok(consent_snapshot_value())
        }
        Err(e) => Reply::err_msg("bad_request", e),
    }
}

async fn consent_stream_pending_impl(payload: Value, sender: StreamSender) {
    // Heartbeat keeps the pipe stream warm so the GUI's HTTP idle
    // timeout can't time out a long-lived "no pending prompts"
    // session. Configurable for tests; default matches the Wylde user's
    // GUI-side keepalive.
    let heartbeat_secs = payload
        .get("heartbeat_secs")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
        .unwrap_or(30);
    let heartbeat = std::time::Duration::from_secs(heartbeat_secs);

    let (mut rx, snapshot) = consent::subscribe_pending();

    // Emit existing pending entries first so a tab that opens after a
    // prompt fired still sees the toast.
    for entry in snapshot {
        if sender.send(Ok(pending_event_chunk(&entry))).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            biased;
            _ = sender.closed() => {
                // Client dropped the receiver — exit cleanly so the
                // broadcast subscription drops with the task.
                return;
            }
            ev = rx.recv() => {
                match ev {
                    Ok(consent::ConsentEvent::Pending(entry)) => {
                        if sender.send(Ok(pending_event_chunk(&entry))).await.is_err() {
                            return;
                        }
                    }
                    Ok(consent::ConsentEvent::Resolved { id, tool, decision }) => {
                        let chunk = json!({
                            "type": "resolved",
                            "id": id,
                            "tool": tool,
                            "decision": decision,
                        });
                        if sender.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Buffer overran — the GUI will refetch the
                        // full pending list via `consent.list` so
                        // skipping this chunk is safe. Tell the
                        // client so it knows to recover.
                        let chunk = json!({"type": "lagged"});
                        if sender.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Process-wide broadcaster never closes
                        // (static OnceLock), but exit cleanly anyway.
                        return;
                    }
                }
            }
            _ = tokio::time::sleep(heartbeat) => {
                let chunk = json!({
                    "type": "heartbeat",
                    "ts": chrono::Utc::now().timestamp(),
                });
                if sender.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn pending_event_chunk(entry: &consent::PendingEntry) -> Value {
    json!({
        "type": "pending",
        "id": entry.id,
        "tool": entry.tool,
        "summary": entry.summary,
        "default_action": entry.default_action,
        "awaiting_since": entry.awaiting_since,
    })
}

// ── Shared JSON helpers ──────────────────────────────────────────────
// Pre-Phase-12.1 these lived in pipe/mod.rs as `pub(crate)`. The trait
// methods are the only callers now, so they move adjacent to those
// methods.

pub(crate) fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

pub(crate) fn optional_string(payload: &Value, key: &str) -> Option<String> {
    require_string(payload, key)
}

fn record_to_value(record: LongTermMemory) -> Value {
    serde_json::to_value(record).expect("LongTermMemory serializes to JSON")
}

fn string_array(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;

    // ── tools.* unit tests (moved from pipe/tools.rs) ────────────────

    #[tokio::test]
    async fn tools_list_returns_catalog_with_count() {
        let api = DefaultHarnessApi;
        let reply = api.tools_list(Value::Null).await;
        assert!(reply.ok);
        let tools = reply.data["tools"].as_array().expect("array");
        let count = reply.data["count"].as_u64().expect("count is uint");
        assert_eq!(count as usize, tools.len());
        let ids: Vec<&str> = tools.iter().filter_map(|t| t["id"].as_str()).collect();
        assert!(
            ids.contains(&"time_now"),
            "expected `time_now` in catalog, got {ids:?}"
        );
    }

    #[tokio::test]
    async fn tools_list_entries_carry_status_and_destructive_flag() {
        let api = DefaultHarnessApi;
        let reply = api.tools_list(Value::Null).await;
        assert!(reply.ok);
        let first = &reply.data["tools"][0];
        assert!(first["status"].is_string(), "status must be a string");
        assert!(
            first["destructive"].is_boolean(),
            "destructive must be a bool"
        );
    }

    #[tokio::test]
    async fn tools_run_rejects_missing_name() {
        let api = DefaultHarnessApi;
        let reply = api.tools_run(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn tools_run_rejects_non_map_args() {
        let api = DefaultHarnessApi;
        let reply = api
            .tools_run(json!({"name": "time.now", "args": "oops"}))
            .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn tools_run_dispatches_active_tool_and_returns_ok_envelope() {
        // Phase 12.2 consent gate guards every dispatch; bypass it
        // here under the shared serial guard so the existing
        // tools.run semantics keep being pinned. New consent
        // integration tests live in `tooling::runner::tests`.
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::set_bypass_for_tests(true);
        let api = DefaultHarnessApi;
        let reply = api.tools_run(json!({"name": "time.now"})).await;
        assert!(reply.ok, "outer envelope is ok");
        assert_eq!(reply.data["ok"], true);
        assert_eq!(reply.data["canonical_id"], "time_now");
        assert_eq!(reply.data["data"]["status"], "success");
    }

    #[tokio::test]
    async fn tools_run_returns_not_found_for_unknown_tool() {
        let api = DefaultHarnessApi;
        let reply = api
            .tools_run(json!({"name": "definitely.not.a.tool"}))
            .await;
        assert!(reply.ok, "outer envelope is ok (transport-level)");
        assert_eq!(reply.data["ok"], false);
        assert_eq!(reply.data["error"]["code"], "not_found");
    }

    // ── rag.* trait-method tests (Wylde_Study S2a) ──────────────────
    // Exercise the api-layer wrappers: validation surfaces as a
    // `status`-envelope inside an ok Reply, and an add → search
    // round-trip works through the trait with precomputed vectors (no
    // live wylde-ollama needed).

    fn set_embed_dim_4() {
        std::env::set_var("WYLDE_EMBED_DIM", "4");
    }

    #[tokio::test]
    async fn rag_add_episodic_rejects_missing_content() {
        let _env = TestEnv::new();
        set_embed_dim_4();
        let api = DefaultHarnessApi;
        let reply = api.rag_add_episodic(json!({"url": "http://x"})).await;
        assert!(reply.ok, "transport-level ok");
        assert_eq!(reply.data["status"], "error");
    }

    #[tokio::test]
    async fn rag_search_rejects_missing_q() {
        let _env = TestEnv::new();
        set_embed_dim_4();
        let api = DefaultHarnessApi;
        let reply = api
            .rag_search(json!({"query_vector": [1.0, 0.0, 0.0, 0.0]}))
            .await;
        assert!(reply.ok);
        assert_eq!(reply.data["status"], "error");
    }

    #[tokio::test]
    async fn rag_add_episodic_then_search_round_trips_via_trait() {
        let _env = TestEnv::new();
        set_embed_dim_4();
        let api = DefaultHarnessApi;

        let added = api
            .rag_add_episodic(json!({
                "content": "trait-path episodic body",
                "url": "http://x/page",
                "vector": [1.0, 0.0, 0.0, 0.0],
            }))
            .await;
        assert!(added.ok);
        assert_eq!(added.data["status"], "ok");
        let id = added.data["memory_id"].as_str().unwrap().to_owned();

        let found = api
            .rag_search(json!({
                "q": "trait body",
                "query_vector": [1.0, 0.0, 0.0, 0.0],
            }))
            .await;
        assert!(found.ok);
        assert_eq!(found.data["status"], "ok");
        let results = found.data["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["id"], id);
    }

    // ── Tool-registry consolidation Slice 1/2 — verb-tool smoke tests ──
    //
    // The eight verb tools co-exist with the old named tools in the
    // catalog. These prove the full `tools.run` → runner → tier gate →
    // consent gate → verb handler → ResourceRegistry path returns valid
    // JSON. Slice 2 lights up the `memory` resource, so describe now
    // surfaces it through the full pipeline.

    #[tokio::test]
    async fn tools_run_dispatches_wylde_describe_through_full_pipeline() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::set_bypass_for_tests(true);
        let api = DefaultHarnessApi;
        let reply = api.tools_run(json!({"name": "wylde_describe"})).await;
        assert!(reply.ok, "outer envelope is ok");
        assert_eq!(reply.data["ok"], true);
        assert_eq!(reply.data["canonical_id"], "wylde_describe");
        assert_eq!(reply.data["data"]["status"], "success");
        // Slice 2: the `memory` resource is registered, so describe lists it.
        let rows = reply.data["data"]["resources"].as_array().unwrap();
        assert!(
            rows.iter().any(|r| r["resource_type"] == "memory"),
            "describe should surface the memory resource through the full pipeline",
        );
        assert_eq!(reply.data["data"]["count"].as_u64().unwrap(), rows.len() as u64);
        crate::tooling::consent::set_bypass_for_tests(false);
    }

    #[tokio::test]
    async fn tools_run_wylde_list_unknown_resource_is_clean_not_found() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::set_bypass_for_tests(true);
        let api = DefaultHarnessApi;
        let reply = api
            .tools_run(json!({"name": "wylde_list", "args": {"resource_type": "nope"}}))
            .await;
        assert!(reply.ok);
        // Transport + dispatch succeed; the verb returns a structured
        // not-found envelope (not a hard error) so the model can recover.
        assert_eq!(reply.data["ok"], true);
        assert_eq!(reply.data["data"]["status"], "not_found");
        assert_eq!(reply.data["data"]["op"], "list");
        crate::tooling::consent::set_bypass_for_tests(false);
    }

    #[tokio::test]
    async fn tools_run_wylde_create_then_get_memory_full_pipeline() {
        // Slice 2: the memory verb path end-to-end through the runner —
        // `wylde_create` (destructive, consent-gated) then `wylde_get`.
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::set_bypass_for_tests(true);
        let _env = TestEnv::new();
        std::env::set_var("WYLDE_EMBED_DIM", "3");
        let api = DefaultHarnessApi;

        // create is destructive → needs the destructive tier (consent is
        // bypassed above; the tier gate is separate).
        let created = api
            .tools_run(json!({
                "name": "wylde_create",
                "device_tier": "destructive_tool_access",
                "args": {"resource_type": "memory", "body": {"body": "pipeline memory", "importance": 7}},
            }))
            .await;
        assert!(created.ok);
        assert_eq!(created.data["ok"], true);
        assert_eq!(created.data["canonical_id"], "wylde_create");
        assert_eq!(created.data["data"]["status"], "success");
        let id = created.data["data"]["id"].as_str().unwrap().to_owned();

        let got = api
            .tools_run(json!({
                "name": "wylde_get",
                "args": {"resource_type": "memory", "resource_id": id},
            }))
            .await;
        assert!(got.ok);
        assert_eq!(got.data["data"]["status"], "success");
        assert_eq!(got.data["data"]["memory"]["body"], "pipeline memory");
        crate::tooling::consent::set_bypass_for_tests(false);
    }

    // ── memory.long_term.* unit tests (moved from pipe/memory_long_term.rs) ──

    #[tokio::test]
    async fn long_term_list_empty_returns_zero_count() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_list(Value::Null).await;
        assert!(reply.ok);
        assert_eq!(reply.data["count"], 0);
        assert_eq!(reply.data["memories"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn long_term_save_rejects_missing_body() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_save(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn long_term_save_rejects_blank_body() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_save(json!({"body": ""})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn long_term_save_persists_record_and_returns_dict() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api
            .memory_long_term_save(json!({
                "body": "remember the alamo",
                "source": "settings_ui",
                "importance": 7,
                "tags": ["history", "tx"],
            }))
            .await;
        assert!(reply.ok, "save should succeed: {reply:?}");
        assert_eq!(reply.data["body"], "remember the alamo");
        assert_eq!(reply.data["source"], "settings_ui");
        assert_eq!(reply.data["importance"], 7);
        let tags = reply.data["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 2);

        let list = api.memory_long_term_list(Value::Null).await;
        assert_eq!(list.data["count"], 1);
    }

    #[tokio::test]
    async fn long_term_save_accepts_float_importance() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api
            .memory_long_term_save(json!({
                "body": "float importance",
                "importance": 6.5_f64,
            }))
            .await;
        assert!(reply.ok);
        let imp = reply.data["importance"].as_i64().expect("integer");
        assert!((1..=10).contains(&imp), "importance out of range: {imp}");
    }

    #[tokio::test]
    async fn long_term_update_returns_not_found_for_unknown_id() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api
            .memory_long_term_update(json!({"id": "deadbeef", "body": "x"}))
            .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn long_term_update_rejects_missing_id() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_update(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn long_term_update_supersedes_existing_record() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let saved = api.memory_long_term_save(json!({"body": "v1"})).await;
        let id = saved.data["id"].as_str().unwrap().to_owned();

        let updated = api
            .memory_long_term_update(json!({"id": id, "body": "v2"}))
            .await;
        assert!(updated.ok);
        assert_eq!(updated.data["body"], "v2");
        let new_id = updated.data["id"].as_str().unwrap();
        assert_ne!(new_id, id, "update must mint a new record id");
    }

    #[tokio::test]
    async fn long_term_delete_returns_ok_false_for_unknown_id() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_delete(json!({"id": "deadbeef"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["ok"], false);
    }

    #[tokio::test]
    async fn long_term_delete_removes_existing_record() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let saved = api.memory_long_term_save(json!({"body": "to delete"})).await;
        let id = saved.data["id"].as_str().unwrap().to_owned();
        let del = api
            .memory_long_term_delete(json!({"id": id.clone()}))
            .await;
        assert_eq!(del.data["ok"], true);
        assert_eq!(del.data["id"], id);

        let list = api.memory_long_term_list(Value::Null).await;
        assert_eq!(list.data["count"], 0);
    }

    #[tokio::test]
    async fn long_term_delete_rejects_missing_id() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_delete(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn long_term_history_rejects_missing_id() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api.memory_long_term_history(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn long_term_history_returns_empty_chain_for_unknown_id() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let reply = api
            .memory_long_term_history(json!({"id": "deadbeef"}))
            .await;
        assert!(reply.ok);
        assert_eq!(reply.data["id"], "deadbeef");
        assert_eq!(reply.data["chain"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn long_term_history_walks_chain_after_update() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let saved = api.memory_long_term_save(json!({"body": "v1"})).await;
        let v1_id = saved.data["id"].as_str().unwrap().to_owned();
        let updated = api
            .memory_long_term_update(json!({"id": v1_id, "body": "v2"}))
            .await;
        let v2_id = updated.data["id"].as_str().unwrap().to_owned();

        let reply = api
            .memory_long_term_history(json!({"id": v2_id.clone()}))
            .await;
        assert!(reply.ok);
        let chain = reply.data["chain"].as_array().unwrap();
        assert_eq!(chain.len(), 2);
        let bodies: Vec<&str> = chain
            .iter()
            .map(|v| v["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, vec!["v1", "v2"]);
    }

    #[tokio::test]
    async fn long_term_list_excludes_superseded_by_default() {
        let _env = TestEnv::new();
        let api = DefaultHarnessApi;
        let saved = api.memory_long_term_save(json!({"body": "v1"})).await;
        let v1_id = saved.data["id"].as_str().unwrap().to_owned();
        let _ = api
            .memory_long_term_update(json!({"id": v1_id, "body": "v2"}))
            .await;

        let default_list = api.memory_long_term_list(Value::Null).await;
        assert_eq!(default_list.data["count"], 1);

        let with_superseded = api
            .memory_long_term_list(json!({"include_superseded": true}))
            .await;
        assert_eq!(with_superseded.data["count"], 2);
    }

    // ── consent.* unit tests (Phase 12.2) ────────────────────────────
    //
    // All consent tests acquire the shared serial guard and write to a
    // tempdir-backed store so they don't corrupt the host's real
    // `data/preferences/consent.json` or race against the gate
    // integration tests in `tooling::runner::tests`.

    async fn consent_test_scope<F, Fut>(test_body: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _g = crate::tooling::consent::serial_test_guard().await;
        // Bypass irrelevant here — we're testing the consent.* verbs
        // themselves, not the gate they affect — but we still need to
        // ensure the global store is pointed at a tempdir so the
        // writes don't persist to the host.
        let td = tempfile::TempDir::new().expect("tempdir");
        crate::tooling::consent::store_set_path_for_tests_helper(
            crate::tooling::consent::store(),
            td.path().join("consent.json"),
        );
        crate::tooling::consent::store()
            .reset()
            .expect("reset injected store");
        test_body().await;
    }

    #[tokio::test]
    async fn consent_list_returns_empty_shape_on_fresh_install() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api.consent_list(Value::Null).await;
            assert!(reply.ok);
            assert_eq!(reply.data["no_auth"], false);
            assert_eq!(
                reply.data["tools"].as_object().unwrap().len(),
                0,
                "fresh store has no per-tool decisions"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_persists_and_shows_in_list() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({"tool_id": "fs.write_file", "decision": "approved"}))
                .await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["tools"]["fs.write_file"], "approved");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_rejects_unknown_decision() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api
                .consent_set(json!({"tool_id": "fs.write_file", "decision": "maybe"}))
                .await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_rejects_missing_tool_id() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api.consent_set(json!({"decision": "approved"})).await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_respond_writes_through_same_path_as_set() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_respond(
                    json!({"tool_id": "fs.read_file", "decision": "denied"}),
                )
                .await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["tools"]["fs.read_file"], "denied");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_clear_returns_tool_to_pending() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({"tool_id": "fs.write_file", "decision": "approved"}))
                .await;
            let _ = api.consent_clear(json!({"tool_id": "fs.write_file"})).await;
            let listed = api.consent_list(Value::Null).await;
            assert!(listed.data["tools"].as_object().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_no_auth_flips_global_flag() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api.consent_set_no_auth(json!({"enabled": true})).await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["no_auth"], true);
            let _ = api.consent_set_no_auth(json!({"enabled": false})).await;
            let listed2 = api.consent_list(Value::Null).await;
            assert_eq!(listed2.data["no_auth"], false);
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_no_auth_rejects_non_bool() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api.consent_set_no_auth(json!({"enabled": "yes"})).await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_reset_clears_everything() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({"tool_id": "alpha", "decision": "approved"}))
                .await;
            let _ = api.consent_set_no_auth(json!({"enabled": true})).await;
            let _ = api.consent_reset(Value::Null).await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["no_auth"], false);
            assert!(listed.data["tools"].as_object().unwrap().is_empty());
        })
        .await;
    }

    // ── Phase 12.6: one-time grants + streaming ──────────────────────

    #[tokio::test]
    async fn consent_set_with_remember_true_writes_to_disk() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            // Default remember is true; explicit `remember: true` is
            // identical.
            let _ = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": true,
                }))
                .await;
            let snap = crate::tooling::consent::store().snapshot();
            assert_eq!(
                snap.tools.get("fs.write_file"),
                Some(&crate::tooling::consent::Decision::Approved),
                "remember:true persists to the file shape"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_with_remember_false_does_not_persist() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": false,
                }))
                .await;
            assert!(reply.ok, "one-time grant returns ok");
            let snap = crate::tooling::consent::store().snapshot();
            assert!(
                !snap.tools.contains_key("fs.write_file"),
                "remember:false must NOT write to disk; got {:?}",
                snap.tools
            );
            // The consent.list reply mirrors the on-disk shape, so
            // the per-tool map should be empty too.
            let listed = api.consent_list(Value::Null).await;
            assert!(
                listed.data["tools"]
                    .as_object()
                    .map(|m| m.is_empty())
                    .unwrap_or(false),
                "consent.list should show no per-tool entries"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_with_remember_false_resolves_current_call_then_returns_pending() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": false,
                }))
                .await;
            // The one-time approval is consumed by the next gate
            // check.
            let outcome = crate::tooling::consent::store().check("fs.write_file", || "p".into());
            assert_eq!(outcome, crate::tooling::consent::GateOutcome::Allow);
            // Second check → no record → pending.
            let outcome2 = crate::tooling::consent::store().check("fs.write_file", || "p".into());
            assert!(matches!(
                outcome2,
                crate::tooling::consent::GateOutcome::Pending { .. }
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_rejects_non_bool_remember() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": "no",
                }))
                .await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    // ── consent.stream_pending streaming tests ───────────────────────

    /// Drain a single pending event from the receiver. Skips any
    /// initial-snapshot frames whose ids don't match `id`.
    async fn next_chunk(
        rx: &mut tokio::sync::mpsc::Receiver<
            Result<Value, wylde_shared::ipc::IpcError>,
        >,
    ) -> Value {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("frame arrives within 1s")
            .expect("stream still open");
        frame.expect("chunk is Ok")
    }

    #[tokio::test]
    async fn consent_stream_pending_emits_event_on_record_pending() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let api = DefaultHarnessApi;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        // Give the spawned task time to subscribe before we record.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let id = crate::tooling::consent::record_pending(
            "fs.write_file",
            "writes a file".into(),
            "deny",
        );
        let chunk = next_chunk(&mut rx).await;
        assert_eq!(chunk["type"], "pending");
        assert_eq!(chunk["id"], id);
        assert_eq!(chunk["tool"], "fs.write_file");
        assert_eq!(chunk["default_action"], "deny");
        assert!(chunk["summary"].is_string());
        assert!(chunk["awaiting_since"].is_i64());
        // Drop the receiver — handler should exit cleanly.
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s after receiver drops")
            .expect("join ok");
        crate::tooling::consent::clear_pending();
    }

    #[tokio::test]
    async fn consent_stream_pending_emits_resolved_on_consent_set() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let td = tempfile::TempDir::new().expect("tempdir");
        crate::tooling::consent::store_set_path_for_tests_helper(
            crate::tooling::consent::store(),
            td.path().join("consent.json"),
        );
        crate::tooling::consent::store().reset().expect("reset");

        let api = DefaultHarnessApi;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let id = crate::tooling::consent::record_pending(
            "fs.write_file",
            "writes a file".into(),
            "deny",
        );
        // Drain pending event.
        let pending = next_chunk(&mut rx).await;
        assert_eq!(pending["type"], "pending");

        let _ = api
            .consent_set(json!({
                "tool_id": "fs.write_file",
                "decision": "approved",
            }))
            .await;
        let resolved = next_chunk(&mut rx).await;
        assert_eq!(resolved["type"], "resolved");
        assert_eq!(resolved["id"], id);
        assert_eq!(resolved["tool"], "fs.write_file");
        assert_eq!(resolved["decision"], "approved");

        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s")
            .expect("join ok");
        crate::tooling::consent::clear_pending();
    }

    #[tokio::test]
    async fn consent_stream_pending_emits_snapshot_on_subscribe() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let id = crate::tooling::consent::record_pending(
            "fs.write_file",
            "writes a file".into(),
            "deny",
        );
        let api = DefaultHarnessApi;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        let chunk = next_chunk(&mut rx).await;
        assert_eq!(chunk["type"], "pending");
        assert_eq!(chunk["id"], id);
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s")
            .expect("join ok");
        crate::tooling::consent::clear_pending();
    }

    #[tokio::test]
    async fn consent_stream_pending_closes_cleanly_on_client_drop() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let api = DefaultHarnessApi;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        // Give the spawn time to enter its select loop.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Client disconnects.
        drop(rx);
        // Handler must observe sender.closed() and exit.
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s after client drops")
            .expect("join ok");
    }
}
