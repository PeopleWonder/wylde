//! Pipe-action dispatcher — registers every GUI-facing harness verb on
//! the process-wide IPC registry.
//!
//! Pre-Phase-12.1 this was a four-file module (`pipe/chat.rs`,
//! `pipe/tools.rs`, `pipe/memory_long_term.rs`, `pipe/memory_workspaces.rs`)
//! that did both (a) verb-name → handler registration and (b) JSON payload
//! validation + reply shaping. Phase 12.1 split those concerns:
//!
//! * The per-verb JSON shaping moved into [`crate::api::HarnessApi`]'s
//!   default impl so the Tauri-side in-process dispatcher can share it.
//! * This file shrank to the registration loop only — one
//!   [`register_action_with_meta`] / [`register_streaming_action_with_meta`]
//!   call per verb, each delegating to the trait method.
//!
//! The harness binary still uses this module unchanged (see
//! [`crate::service::install`]) so external pipe clients (MCP, CLI tools,
//! parity tests) see the same surface they always did. The Tauri side
//! constructs its own [`crate::api::DefaultHarnessApi`] and routes verbs
//! in-process, bypassing the IPC hop entirely.
//!
//! ## Strangler-fig contract
//!
//! Verbs NOT in [`ALL_PIPE_ACTIONS`] are intentionally absent — the IPC
//! dispatcher surfaces them as `no_action`, which the Python strangler's
//! transport-code fallback treats as "revert to in-process Python." A
//! partial port can't brick chat. The deferred punchlist:
//!
//! * `memory.workspace.*` (6 verbs) — `workspace_memory` not in Rust.
//! * `memory.short_term.*` (3 verbs) — `conversation` not in Rust.
//! * `memory.reflect` — `reflection` not in Rust.
//! * `conversations.*` (4 verbs) — `conversation` not in Rust.
//! * `prompts.*` (5 verbs) — `system_prompts` not in Rust.
//! * `rag.workspaces.*` (10 verbs) — overlaps `memory.workspaces.*`;
//!   namespace reconciliation + indexer port pending.
//! * `models.*` (8 verbs) — `ollama_client` / `model_state` / Voice
//!   STT/TTS not in Rust.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, StreamSender,
};

use crate::api::HarnessApi;

const HANDLER_MODULE_CHAT: &str = "wylde_harness::api::DefaultHarnessApi (chat.*)";
const HANDLER_MODULE_TOOLS: &str = "wylde_harness::api::DefaultHarnessApi (tools.*)";
const HANDLER_MODULE_LONG_TERM: &str =
    "wylde_harness::api::DefaultHarnessApi (memory.long_term.*)";
const HANDLER_MODULE_WORKSPACES: &str =
    "wylde_harness::api::DefaultHarnessApi (memory.workspaces.*)";
const HANDLER_MODULE_CONSENT: &str = "wylde_harness::api::DefaultHarnessApi (consent.*)";

/// Every action the harness pipe registers. Tests compare this against
/// `list_action_meta()` to catch a missing registration. Order mirrors
/// the Phase 9 sectioning so the contract emitter produces stable output.
pub const ALL_PIPE_ACTIONS: &[&str] = &[
    // chat.* — turn driver (5 verbs)
    "chat.run_turn",
    "chat.start_turn",
    "chat.cancel",
    "chat.stream_turn",
    "chat.stream_tools",
    // tools.* — direct invocation + catalog (2 verbs)
    "tools.list",
    "tools.run",
    // memory.long_term.* — global memory tier (6 verbs)
    "memory.long_term.list",
    "memory.long_term.save",
    "memory.long_term.update",
    "memory.long_term.delete",
    "memory.long_term.history",
    "memory.long_term.search",
    // memory.workspaces.* — workspace registry (8 verbs; Phase 7.A)
    "memory.workspaces.list",
    "memory.workspaces.recent",
    "memory.workspaces.get",
    "memory.workspaces.get_mru_limit",
    "memory.workspaces.set_mru_limit",
    "memory.workspaces.get_persona",
    "memory.workspaces.set_persona",
    "memory.workspaces.delete",
    // consent.* — per-tool consent gate (Phase 12.2; 6 unary + 1 streaming = 7 verbs)
    "consent.list",
    "consent.set",
    "consent.respond",
    "consent.clear",
    "consent.set_no_auth",
    "consent.reset",
    "consent.stream_pending",
];

/// Register every pipe action against `api` on the process-wide IPC
/// registry. Called from [`crate::service::install`] after the first
/// `INSTALLED` flag flip with a [`crate::api::DefaultHarnessApi`].
///
/// `api` is wrapped in an [`Arc`] so each registered closure can hold
/// its own clone of the shared trait object — the IPC registry stores
/// the closures past the call's stack frame.
pub fn install_all_against<A>(api: A)
where
    A: HarnessApi + 'static,
{
    let api: Arc<dyn HarnessApi> = Arc::new(api);

    // ── chat.* ───────────────────────────────────────────────────────

    let a = Arc::clone(&api);
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

    let a = Arc::clone(&api);
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

    let a = Arc::clone(&api);
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

    let a = Arc::clone(&api);
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

    let a = Arc::clone(&api);
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

    // ── tools.* ──────────────────────────────────────────────────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "tools.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.tools_list(p).await }
        },
        "Return the live tool catalog from the in-process registry. \
         No payload. Returns {tools: [...], count}. Each entry: \
         {id, name, group, description, parameters, destructive, \
         status, deferred_phase}.",
        HANDLER_MODULE_TOOLS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "tools.run",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.tools_run(p).await }
        },
        "Invoke one tool by id/alias. Payload {name, args?, \
         device_tier?}. Returns the dispatch outcome flattened: \
         {ok, data} on success, {ok: false, error: {code, message}} \
         on failure. The tier gate runs against the supplied \
         device_tier (default `tool_use`).",
        HANDLER_MODULE_TOOLS,
    );

    // ── memory.long_term.* ───────────────────────────────────────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.long_term.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_long_term_list(p).await }
        },
        "Return every long-term record, importance-desc then \
         recency-desc. Payload {include_superseded?}. Returns \
         {memories: [...], count}.",
        HANDLER_MODULE_LONG_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.long_term.save",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_long_term_save(p).await }
        },
        "Persist a new long-term record. Payload {body, source?, \
         importance?, tags?}. Returns the new record as JSON.",
        HANDLER_MODULE_LONG_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.long_term.update",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_long_term_update(p).await }
        },
        "Revise an existing record (writes a new record and marks the \
         old one superseded). Payload {id, body?, source?, \
         importance?}. Returns the replacement record.",
        HANDLER_MODULE_LONG_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.long_term.delete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_long_term_delete(p).await }
        },
        "Permanently remove a record (and anything superseded by it). \
         Payload {id}. Returns {ok, id}.",
        HANDLER_MODULE_LONG_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.long_term.history",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_long_term_history(p).await }
        },
        "Walk the supersession chain. Payload {id}. Returns \
         {id, chain: [LongTermMemory...]} oldest-to-newest.",
        HANDLER_MODULE_LONG_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.long_term.search",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_long_term_search(p).await }
        },
        "Vector + recency-decay search over long-term memory. Payload \
         {query, limit?, decay_days?}. Embeds `query` via wylde-ollama, \
         then ranks records by similarity boosted by importance + \
         recency decay. Superseded records are filtered out. Returns \
         {results: [SearchHit...]}.",
        HANDLER_MODULE_LONG_TERM,
    );

    // ── memory.workspaces.* ──────────────────────────────────────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_list(p).await }
        },
        "Every workspace in MRU order. Returns {workspaces: [Workspace, ...]}.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.recent",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_recent(p).await }
        },
        "First N workspaces in MRU order. Payload {limit?}; default is \
         the user-configured MRU cap.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_get(p).await }
        },
        "One workspace by id. Payload {workspace_id}. Returns \
         the Workspace JSON or not_found.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.get_mru_limit",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_get_mru_limit(p).await }
        },
        "Current MRU cap + bounds. Returns {limit, min, max, default}.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.set_mru_limit",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_set_mru_limit(p).await }
        },
        "Persist a new MRU cap; immediately evicts any workspaces past \
         the new cap (index folders removed, durable workspace memory \
         preserved). Payload {limit}.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.get_persona",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_get_persona(p).await }
        },
        "Persona text for a workspace. Payload {workspace_id}.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.set_persona",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_set_persona(p).await }
        },
        "Persist persona text for a workspace. Payload {workspace_id, text?}.",
        HANDLER_MODULE_WORKSPACES,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.workspaces.delete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspaces_delete(p).await }
        },
        "Explicit delete: removes from registry, deletes the on-disk \
         index folder AND the durable workspace memory folder. Payload \
         {workspace_id}.",
        HANDLER_MODULE_WORKSPACES,
    );

    // ── consent.* (Phase 12.2) ───────────────────────────────────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "consent.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.consent_list(p).await }
        },
        "Return the persisted consent shape. No payload. Reply: \
         {no_auth, tools: {tool_id: \"approved\"|\"denied\"}}.",
        HANDLER_MODULE_CONSENT,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "consent.set",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.consent_set(p).await }
        },
        "Persist a per-tool decision. Payload {tool_id, decision: \
         \"approved\"|\"denied\"}. Reply: snapshot after the write.",
        HANDLER_MODULE_CONSENT,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "consent.respond",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.consent_respond(p).await }
        },
        "GUI response to a pending consent prompt. Same payload + \
         reply as `consent.set`; the verb name pins this as the \
         response-to-prompt path for future prompt-correlation work.",
        HANDLER_MODULE_CONSENT,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "consent.clear",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.consent_clear(p).await }
        },
        "Drop a per-tool decision (returns the tool to \"pending\" \
         on next dispatch). Payload {tool_id}. Reply: snapshot.",
        HANDLER_MODULE_CONSENT,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "consent.set_no_auth",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.consent_set_no_auth(p).await }
        },
        "Toggle the global no-auth flag. When enabled, every tool \
         is approved without prompting. Payload {enabled: bool}.",
        HANDLER_MODULE_CONSENT,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "consent.reset",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.consent_reset(p).await }
        },
        "Reset the consent store to defaults (no_auth=false, no \
         per-tool decisions). No payload. Reply: empty snapshot.",
        HANDLER_MODULE_CONSENT,
    );

    let a = Arc::clone(&api);
    register_streaming_action_with_meta(
        "consent.stream_pending",
        move |p: Value, sender: StreamSender| {
            let a = Arc::clone(&a);
            async move {
                a.consent_stream_pending(p, sender).await;
            }
        },
        "Streaming. Subscribe to pending-consent events (Phase 12.6). \
         Emits one chunk per pending dispatch (`type: \"pending\"` with \
         {id, tool, summary, default_action, awaiting_since}), one \
         `type: \"resolved\"` chunk when the user picks a decision via \
         consent.set / consent.respond / consent.clear, periodic \
         `type: \"heartbeat\"` chunks every `heartbeat_secs` seconds \
         (default 30), and `type: \"lagged\"` if the broadcast buffer \
         overran. Payload: {heartbeat_secs?: u64}. Stream closes when \
         the client disconnects.",
        HANDLER_MODULE_CONSENT,
    );
}

/// Backwards-compat shim — equivalent to
/// `install_all_against(DefaultHarnessApi)`. The harness binary uses
/// this from [`crate::service::install`]; callers that want a custom
/// `HarnessApi` (mock for tests, instrumentation wrapper) should call
/// [`install_all_against`] directly.
pub fn install_all() {
    install_all_against(crate::api::DefaultHarnessApi);
}
