//! `chat.*` pipe registrations -- turn driver, scoped history search
//! (TBS Slice E) and export/import (TBS Slice J). Split from `pipe.rs`
//! per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, StreamSender,
};

use crate::api::HarnessApi;

const HANDLER_MODULE_CHAT: &str = "wylde_harness::api::DefaultHarnessApi (chat.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── chat.* ───────────────────────────────────────────────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.run_turn",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_run_turn(p).await }
        },
        "Synchronous chat turn — calls wylde-ollama, returns final \
         message. Slice 5.A: single-round, no tool decode, no memory \
         layer. Payload: {user_message, conversation_id, model?, \
         turn_id?, workspace_id?, modality?, device_tier?}.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.preview_context",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_preview_context(p).await }
        },
        "Concept-routing R2 Phase 1 (curate-before-inject). Routes the \
         turn's query into concept space and returns the candidate menu \
         the GUI curates — no injection, no LLM. Payload {user_message, \
         conversation_id?, workspace_id?, excluded_tokens?, \
         reactivated_tokens?, active_file?}. Returns {routing_enabled, \
         curate, candidates, inject_token_budget}; candidates is null when \
         the master toggle is OFF or nothing routed (the turn then runs as \
         today). The user-curated ids ride back on chat.run_turn's \
         curated_concepts.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.complete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_complete(p).await }
        },
        "Single-shot LLM completion for extensions (Wylde_Study S2a). \
         Routes through the same ollama.chat pipeline as chat.run_turn \
         (broker lease, model resolution, priority all apply) but sends \
         exactly one user message — no system prompt, no tools field, no \
         tool decode, no conversation history. Payload: {prompt, model?, \
         max_tokens?}. Returns {text, model_used, tokens_used, \
         prompt_tokens, completion_tokens}.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.start_turn",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_start_turn(p).await }
        },
        "Non-blocking turn kick-off. Registers a turn handle, spawns \
         the streaming driver task, returns turn_id immediately. \
         Caller follows up with chat.stream_turn / chat.stream_tools. \
         Slice 5.B.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.cancel",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_cancel(p).await }
        },
        "Cancel an in-flight turn by id. Flips the per-turn cancel \
         flag; the driver observes it between Ollama chunks. Returns \
         {turn_id, cancelled} where cancelled is true iff this call \
         actually flipped the flag. Slice 5.B.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_streaming_action_with_meta(
        "chat.stream_turn",
        move |p: Value, sender: StreamSender| {
            let a = Arc::clone(&a);
            async move {
                a.chat_stream_turn(p, sender).await;
            }
        },
        "Streaming. Emits user-facing TurnEvent chunks (token / \
         thinking / turn_complete / turn_aborted) for a turn started \
         with chat.start_turn. Each chunk's wire shape matches \
         Python's per-event long-poll envelope: {type, turn_id, ...}.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_streaming_action_with_meta(
        "chat.stream_tools",
        move |p: Value, sender: StreamSender| {
            let a = Arc::clone(&a);
            async move {
                a.chat_stream_tools(p, sender).await;
            }
        },
        "Streaming. Emits tool-activity ToolEvent chunks (dispatched / \
         result / error / memory_written / warning) for a turn. Slice \
         5.B: tool decode/dispatch isn't wired yet, so this stream \
         emits nothing until 5.C lands the salvage-parser port.",
        HANDLER_MODULE_CHAT,
    );

    // ── chat.* history search (Thought Bubble System Slice E) ─────────
    // Scoped recall over past conversations. The strict workspace boundary
    // (Plan v2 §3.2) is enforced in chat::search::scope; these handlers
    // never read a store the resolver didn't authorise.

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.search_history",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_search_history(p).await }
        },
        "Semantic + lexical search over past conversations, STRICTLY \
         scope-bounded (standalone chat sees standalone only; workspace \
         chat sees its own workspace + standalone; never another \
         workspace). Payload {query, date_range?:{from,to}, \
         workspace_scope?:(\"current\"|\"standalone\"|{workspace_only:id}), \
         active_workspace_id?, top_k?, threshold?}. Returns {hits, count, \
         degraded}. A WorkspaceOnly id outside the current workspace is a \
         bad_request — callers can't escape scope by passing an id.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.list_recent",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_list_recent(p).await }
        },
        "Most-recent conversations in the current scope, newest-first. \
         Payload {limit?, date_range?, workspace_scope?, \
         active_workspace_id?}. Returns {hits, count, degraded}; each hit \
         scores 1.0. Same strict scope boundary as chat.search_history.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.get_conversation",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_get_conversation(p).await }
        },
        "Fetch one conversation document by id, scope-checked: a \
         conversation in another workspace is not_found (the boundary \
         holds for point reads too). Payload {id, workspace_scope?, \
         active_workspace_id?}. Returns the full conversation document.",
        HANDLER_MODULE_CHAT,
    );

    // ── chat.* export / import (Thought Bubble System Slice J) ────────
    // The escape hatch: standalone conversations served in-process from
    // the flat store; a payload `workspace_id` forwards to the
    // wylde-workspaces chat.export/chat.import verbs (Appendix A owners).

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.export",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_export(p).await }
        },
        "Export one conversation as a portable plaintext envelope \
         (wylde-conversation-export v1). Payload {conversation_id, \
         workspace_id?} — workspace_id present forwards to the workspaces \
         service, absent reads the standalone flat store. Reply: \
         {export, id}. The caller persists the file.",
        HANDLER_MODULE_CHAT,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "chat.import",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.chat_import(p).await }
        },
        "Import a portable conversation envelope. Payload {export, \
         workspace_id?, overwrite?} — workspace_id targets that workspace \
         via the service, absent lands it in the standalone store. \
         already_exists on an id collision unless overwrite:true. Reply: \
         {imported, workspace_id}.",
        HANDLER_MODULE_CHAT,
    );
}
