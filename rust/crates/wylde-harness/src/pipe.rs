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
//! * `memory.reflect` — `reflection` not in Rust.
//! * `prompts.*` (5 verbs) — `system_prompts` not in Rust.
//! * `rag.workspaces.*` (10 verbs) — overlaps `memory.workspaces.*`;
//!   namespace reconciliation + indexer port pending.
//!
//! (`models.transcribe` / `models.synthesize` were retired at the voice
//! cutover and deleted in the Bucket-A IPC cleanup — STT/TTS run
//! in-process in `wylde-voice`, reached via `voice.*`. They are not part
//! of this surface.)
//!
//! Harness Slice 3a registered the other eight `models.*` verbs (list,
//! get_profile, show, delete, unload, set_active, set_default,
//! get_default), gated behind `WYLDE_HARNESS_MODELS_IMPL=rust` so the
//! Python implementation stays authoritative until Slice 3b forwards.
//!
//! The three `memory.short_term.*` verbs (`get` / `append` / `clear`)
//! are now registered (Rust port of the working-memory half of
//! `Core/harness/memory/conversation.py`); the Python `_memory.py`
//! handlers became thin forwarders to this pipe, mirroring the chat.*
//! Phase 5.D cutover.
//!
//! Memory Slice B then ported the conversation-lifecycle half of the same
//! file: the six `conversations.*` verbs (`new` / `list` / `get` /
//! `delete` + the net-new `get_active` / `set_active` selection-persistence
//! pair) are registered here, and the Python `_conversations.py` handlers
//! became thin forwarders too. The remaining Python conversation surface
//! (`memory.reflect`, plus `save_conversation` itself) still shares the
//! same JSON files, so both the short-term merge-save and the Slice B
//! read/list/delete path preserve every sibling field.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, StreamSender,
};

use crate::api::HarnessApi;

const HANDLER_MODULE_CHAT: &str = "wylde_harness::api::DefaultHarnessApi (chat.*)";
const HANDLER_MODULE_TOOLS: &str = "wylde_harness::api::DefaultHarnessApi (tools.*)";
const HANDLER_MODULE_MODELS: &str = "wylde_harness::api::DefaultHarnessApi (models.*)";
const HANDLER_MODULE_SETTINGS: &str =
    "wylde_harness::api::DefaultHarnessApi (settings.ollama.*)";
const HANDLER_MODULE_RAG: &str = "wylde_harness::api::DefaultHarnessApi (rag.*)";
const HANDLER_MODULE_LONG_TERM: &str =
    "wylde_harness::api::DefaultHarnessApi (memory.long_term.*)";
const HANDLER_MODULE_SHORT_TERM: &str =
    "wylde_harness::api::DefaultHarnessApi (memory.short_term.*)";
const HANDLER_MODULE_CONVERSATIONS: &str =
    "wylde_harness::api::DefaultHarnessApi (conversations.*)";
const HANDLER_MODULE_CONSENT: &str = "wylde_harness::api::DefaultHarnessApi (consent.*)";
const HANDLER_MODULE_USER_PROFILE: &str =
    "wylde_harness::api::DefaultHarnessApi (user_profile.*)";
const HANDLER_MODULE_GLOBAL_ANCHORS: &str = "wylde_harness::global_anchors::api (anchors.*)";

/// Every action the harness pipe registers. Tests compare this against
/// `list_action_meta()` to catch a missing registration. Order mirrors
/// the Phase 9 sectioning so the contract emitter produces stable output.
pub const ALL_PIPE_ACTIONS: &[&str] = &[
    // chat.* — turn driver (6 verbs)
    "chat.run_turn",
    "chat.complete",
    "chat.start_turn",
    "chat.cancel",
    "chat.stream_turn",
    "chat.stream_tools",
    // chat.* — scoped chat-history search (3 verbs; Thought Bubble System
    // Slice E). Harness-dispatched: standalone conversations read locally,
    // workspace conversations over the wylde-workspaces pipe. Per Build
    // Order Appendix A these are descriptive tiers (search/get Medium · 2s,
    // list Fast · 500ms, all idempotent reads) with NO client-crate cache —
    // the §7.6 TTL cache applies to `workspaces.*` client verbs, not these
    // in-harness handlers (spec Appendix A wins over the brief's 30/60s, as
    // with Slices B/F/G/N-data).
    "chat.search_history",
    "chat.list_recent",
    "chat.get_conversation",
    // tools.* — direct invocation + catalog (2 verbs)
    "tools.list",
    "tools.run",
    // models.* — registry surface + Ollama-side ops (8 verbs; Slice 3a).
    // transcribe/synthesize were retired at the voice cutover (now
    // voice.* in wylde-voice) and deleted in the Bucket-A IPC cleanup.
    "models.list",
    "models.get_profile",
    "models.show",
    "models.delete",
    "models.unload",
    "models.set_active",
    "models.set_default",
    "models.get_default",
    "models.get_effective",
    // settings.ollama.* — per-model inference override store (4 verbs)
    "settings.ollama.get_overrides",
    "settings.ollama.set_overrides",
    "settings.ollama.clear_override",
    "settings.ollama.list_models_with_overrides",
    // rag.* — episodic write + semantic search (2 verbs; Wylde_Study S2a)
    "rag.add_episodic",
    "rag.search",
    // memory.long_term.* — global memory tier (6 verbs)
    "memory.long_term.list",
    "memory.long_term.save",
    "memory.long_term.update",
    "memory.long_term.delete",
    "memory.long_term.history",
    "memory.long_term.search",
    // workspaces.* — RETIRED from the harness pipe (Thought Bubble System
    // Slice 0d). All workspace verbs now live on the wylde-workspaces
    // service pipe; consumers reach them via the wylde-workspaces-client
    // crate. The harness no longer answers `workspaces.*` (→ no_action).
    // memory.short_term.* — conversation working memory (3 verbs)
    "memory.short_term.get",
    "memory.short_term.append",
    "memory.short_term.clear",
    // conversations.* — lifecycle + active selection + workspace (7 verbs)
    "conversations.new",
    "conversations.list",
    "conversations.get",
    "conversations.delete",
    "conversations.get_active",
    "conversations.set_active",
    "conversations.set_workspace",
    // consent.* — per-tool consent gate (Phase 12.2; 6 unary + 1 streaming = 7 verbs)
    "consent.list",
    "consent.set",
    "consent.respond",
    "consent.clear",
    "consent.set_no_auth",
    "consent.reset",
    "consent.stream_pending",
    // user_profile.* — global user-level facts + LLM-proposed updates
    // (Thought Bubble System Slice D; 6 verbs, all in-process). Per
    // Build Order Appendix A these are in-process harness verbs — served
    // straight out of the local store, with no wylde-workspaces pipe hop
    // and so no wylde-workspaces-client timeout/retry/cache tier. (The
    // brief's "Fast · 30s cache" tiers describe that client crate, which
    // doesn't apply here; spec Appendix A wins, per the Slice B/F/G
    // precedent.)
    "user_profile.get",
    "user_profile.update",
    "user_profile.propose",
    "user_profile.accept",
    "user_profile.reject",
    "user_profile.list_proposals",
    // anchors.* — GLOBAL anchor store (Thought Bubble System Slice N-data,
    // harness half). In-process (user-level, not workspace-scoped). The four
    // CRUD verbs are Build Order §3; the three reads mirror the workspace
    // `workspaces.anchors.*` surface so consumers resolve tokens / do the
    // inverse lookup / traverse the hierarchy symmetrically across both scopes.
    "anchors.list",
    "anchors.create",
    "anchors.update",
    "anchors.delete",
    "anchors.find_by_token",
    "anchors.find_by_target",
    "anchors.list_under",
    // anchors.promote_via_alias — the global promotion landing point for an
    // alias-driven promotion (Slice N-data-aliases). Same shape as
    // anchors.create; the whole anchor (all aliases) lands globally.
    "anchors.promote_via_alias",
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

    // ── chat.* history search (Thought Bubble System Slice E) ─────────
    // Scoped recall over past conversations. The strict workspace boundary
    // (Plan v2 §3.2) is enforced in chat::search::scope; these handlers
    // never read a store the resolver didn't authorise.

    let a = Arc::clone(&api);
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

    let a = Arc::clone(&api);
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

    let a = Arc::clone(&api);
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

    // ── models.* (harness Slice 3a) ──────────────────────────────────
    // Registered unconditionally; each handler self-gates on
    // WYLDE_HARNESS_MODELS_IMPL and returns `not_implemented` until the
    // flag is `rust`, so the Python path stays authoritative by default.

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_list(p).await }
        },
        "Registry view of every known model, optionally filtered by \
         `kind` (llm|stt|tts|vision|embed|wakeword). Merges the HF cache \
         scan, service manifests, the live Ollama tag probe, and routing \
         profiles. Payload {kind?}. Returns {models: [...], count, kind}.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.get_profile",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_get_profile(p).await }
        },
        "Routing profile (backend, backend_model, capabilities, \
         benchmark scores) for a model name. Payload {name}. Returns \
         {name, profile} where profile is {} when unknown.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.show",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_show(p).await }
        },
        "Fetch /api/show metadata for a locally-installed Ollama model \
         via wylde-ollama. Payload {name}. Returns the raw Ollama show \
         payload; `not_found` when the model isn't installed.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.delete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_delete(p).await }
        },
        "Uninstall a model via Ollama /api/delete and drop its cached \
         capability flags. Payload {name}. Returns {ok, name} — ok is \
         false when the model was absent or Ollama was unreachable.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.unload",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_unload(p).await }
        },
        "Evict a model from VRAM (Ollama /api/generate keep_alive=0) and \
         drop its cached capability flags. Payload {name}. Returns \
         {ok, name}.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.set_active",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_set_active(p).await }
        },
        "Persist the inference bar's current model pick to \
         $DATA_DIR/active_model.json. Empty string / null clears it. \
         Payload {model?}. Returns {model} (the persisted value or \"\").",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.set_default",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_set_default(p).await }
        },
        "Persist the user's starred default model to \
         $DATA_DIR/default_model.json. null / empty clears it (reads then \
         fall back to WYLDE_DEFAULT_MODEL). Payload {model?}. Returns \
         {ok, model} where model is null when cleared.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.get_default",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_get_default(p).await }
        },
        "Return the starred default model: persisted choice, else the \
         WYLDE_DEFAULT_MODEL env, else null. No payload. Returns {model}.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "models.get_effective",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_get_effective(p).await }
        },
        "Resolve the model whose defaults apply to the next chat turn: \
         active inference-bar pick → starred default → WYLDE_DEFAULT_MODEL \
         env → null. No payload. Returns {model, source} where source is \
         one of active|default|env|null.",
        HANDLER_MODULE_MODELS,
    );

    // ── settings.ollama.* (per-model inference override store) ────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "settings.ollama.get_overrides",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_get_overrides(p).await }
        },
        "Sparse per-model Ollama inference overrides. Payload {model, \
         profile?}. Returns {model, profile, overrides} where overrides \
         is the sparse map of only the keys the user set (empty when none).",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "settings.ollama.set_overrides",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_set_overrides(p).await }
        },
        "Set/merge one per-model override key. Payload {model, key, value, \
         profile?}. Returns {model, profile, overrides} after the merge.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "settings.ollama.clear_override",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_clear_override(p).await }
        },
        "Delete one per-model override key (the field falls back to its \
         placeholder). Payload {model, key, profile?}. Returns {model, \
         profile, overrides} with the remaining keys.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "settings.ollama.list_models_with_overrides",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_list_models_with_overrides(p).await }
        },
        "List real model tags that have at least one stored override \
         (for the future profiles UI). Payload {profile?}. Returns \
         {profile, models}.",
        HANDLER_MODULE_SETTINGS,
    );

    // ── rag.* (Wylde_Study S2a) ──────────────────────────────────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "rag.add_episodic",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.rag_add_episodic(p).await }
        },
        "Add one raw-text episodic memory row (the Rust port of \
         rag.add_episodic). Writes to the same tiered RAG store \
         rag.search reads, so the row is immediately retrievable. \
         Payload: {content|text, source_path?|url?, session_id?, \
         score?, vector?}. Embeds `content` via wylde-ollama when no \
         `vector` is supplied. Returns {status, memory_id, id, chars, \
         memory_type}.",
        HANDLER_MODULE_RAG,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "rag.search",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.rag_search(p).await }
        },
        "Semantic search over the tiered RAG store. Embeds the query \
         text server-side via wylde-ollama (unlike the model-callable \
         rag.ask tool, which requires a precomputed vector), then runs \
         the same first-party vector search. Payload: {q, query_vector?, \
         limit?, tier?, workspace?}. Returns {status, q, workspace_id, \
         results, count}.",
        HANDLER_MODULE_RAG,
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

    // ── workspaces.* — RETIRED (Thought Bubble System Slice 0d) ──────
    // The harness pipe no longer registers any `workspaces.*` verb. All
    // workspace state lives in the wylde-workspaces service; consumers
    // (the chat turn driver, the GUI panels) reach it over its pipe via
    // the wylde-workspaces-client crate. A stray `workspaces.*` call to
    // the harness pipe now returns `no_action`.

    // ── memory.short_term.* (conversation working memory) ────────────

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.short_term.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_short_term_get(p).await }
        },
        "Rolling working-memory buffer for a conversation — tool calls, \
         files opened, decisions reached, summaries read. Stored as the \
         `working_memory` array inside the conversation JSON document. \
         Payload {conversation_id}. Returns {working_memory, \
         conversation_id}; unknown conversation reads as an empty buffer.",
        HANDLER_MODULE_SHORT_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.short_term.append",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_short_term_append(p).await }
        },
        "Append one freeform working-memory entry (convention {kind, at, \
         data}). Mints a stub conversation if none exists and stamps `at` \
         when absent. Payload {conversation_id, entry}; entry must be a \
         map. Returns {conversation_id, working_memory} after the append.",
        HANDLER_MODULE_SHORT_TERM,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "memory.short_term.clear",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_short_term_clear(p).await }
        },
        "Drop the working-memory buffer (e.g. starting a fresh task in an \
         existing conversation). Other conversation fields are preserved. \
         Payload {conversation_id}. Returns {cleared, conversation_id} — \
         cleared is false when there was nothing to drop.",
        HANDLER_MODULE_SHORT_TERM,
    );

    // ── conversations.* (conversation lifecycle + active selection) ──

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.new",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_new(p).await }
        },
        "Mint a fresh, sortable, filename-safe conversation id \
         (timestamp + random suffix). No payload. Returns {id}.",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_list(p).await }
        },
        "Lightweight metadata for every saved chat, newest-first by \
         updated_at. No payload. Returns {conversations, count} where \
         each entry is {id, title, created_at, updated_at, \
         message_count, working_memory_count, model}.",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_get(p).await }
        },
        "Full conversation document by id. Payload {id}. Returns the \
         stored document (id, title, messages, working_memory, …); \
         bad_request for a missing/invalid id, not_found when absent.",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.delete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_delete(p).await }
        },
        "Remove a conversation file. Payload {id}. Returns {ok, id} — \
         ok is false when the file was already absent; bad_request for \
         an invalid id.",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.get_active",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_get_active(p).await }
        },
        "Read the persisted active-conversation selection (the chat the \
         user was last looking at), stored in \
         <data_dir>/active_conversation.json. No payload. Returns {id} — \
         \"\" when none chosen yet.",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.set_active",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_set_active(p).await }
        },
        "Persist the active-conversation selection so it survives an app \
         restart. Payload {id}; an empty/absent id clears the selection. \
         Returns {id} (the persisted value, \"\" when cleared).",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "conversations.set_workspace",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_set_workspace(p).await }
        },
        "Re-assign a conversation's workspace (mutable binding). Payload \
         {id, workspace_id?}; an empty/absent workspace_id clears the \
         binding. Upserts the document. Returns the updated conversation \
         document; bad_request for a missing/invalid id.",
        HANDLER_MODULE_CONVERSATIONS,
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

    // ── user_profile.* (Thought Bubble System Slice D) ───────────────
    // In-process harness verbs; the profile is read into every turn and
    // edited from the Settings "Profile / Rules" page.

    let a = Arc::clone(&api);
    register_action_with_meta(
        "user_profile.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.user_profile_get(p).await }
        },
        "Read the current user profile. No payload. Returns the profile \
         object: {name, preferences, recurring_topics, style, \
         free_text_rules}.",
        HANDLER_MODULE_USER_PROFILE,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "user_profile.update",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.user_profile_update(p).await }
        },
        "Apply a user edit to the profile (user-edit-wins, OI-18). \
         Payload is the patch (the fields to change), or {patch: {...}}. \
         `preferences` merges key-by-key (null value removes a key); \
         other fields replace. Returns the updated profile.",
        HANDLER_MODULE_USER_PROFILE,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "user_profile.propose",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.user_profile_propose(p).await }
        },
        "An LLM-proposed profile update enters the pending queue, \
         subject to spam control (OI-7: <=10/conversation, 1h per-field \
         cooldown, confidence >=0.7; OI-11: 30-day rejection \
         suppression). Payload: {field, proposed, confidence, current?, \
         rationale?, conversation_id?}. Returns {accepted: true, \
         proposal} on admission, or {accepted: false, reason, message} \
         when the gate refuses (still ok=true).",
        HANDLER_MODULE_USER_PROFILE,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "user_profile.accept",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.user_profile_accept(p).await }
        },
        "Accept a pending proposal — apply it to the profile and drop it \
         from the queue. Payload {proposal_id}. Returns the updated \
         profile.",
        HANDLER_MODULE_USER_PROFILE,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "user_profile.reject",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.user_profile_reject(p).await }
        },
        "Reject a pending proposal — drop it and record it for the OI-11 \
         30-day suppression window. Payload {proposal_id}. Returns \
         {rejected: true, proposal_id}.",
        HANDLER_MODULE_USER_PROFILE,
    );

    let a = Arc::clone(&api);
    register_action_with_meta(
        "user_profile.list_proposals",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.user_profile_list_proposals(p).await }
        },
        "List the pending LLM-proposed profile updates (newest last). No \
         payload. Returns {proposals: [...], count}.",
        HANDLER_MODULE_USER_PROFILE,
    );

    // ── anchors.* — GLOBAL anchor store (Slice N-data, harness half) ──────
    //
    // These are file-backed CRUD over `<data_dir>/global_anchors.json`, not a
    // `HarnessApi` method — they have no shared state to thread through the
    // trait, so they register as plain free-fn closures (like the
    // `wylde-workspaces` service's own action handlers).
    install_global_anchor_actions();
}

/// Register the eight in-process `anchors.*` (global-scope) verbs. Split out so
/// `install_all_against` stays readable; called at the end of it.
fn install_global_anchor_actions() {
    use crate::global_anchors::api as ga;

    register_action_with_meta(
        "anchors.list",
        |p: Value| async move { ga::handle_list(p).await },
        "Every GLOBAL anchor. No payload. Reply: {scope: \"global\", anchors, \
         count}. Same Anchor wire shape as workspaces.anchors.*.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.create",
        |p: Value| async move { ga::handle_create(p).await },
        "Promote/mint a GLOBAL anchor. Payload: {identifier, kind?, target, \
         description?, parent_anchor?, domain?, related_to?}. Reply: the \
         Anchor. OI-5 collision: a duplicate identifier returns \
         `already_exists_global` (details carry the existing definition for \
         the rename/keep/replace dialog); never an overwrite.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.update",
        |p: Value| async move { ga::handle_update(p).await },
        "Patch a GLOBAL anchor's description/target/related_to/parent_anchor/\
         domain. Payload: {identifier, ...patch}. Reply: the updated Anchor. \
         not_found for an unknown identifier.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.delete",
        |p: Value| async move { ga::handle_delete(p).await },
        "Remove a GLOBAL anchor by identifier. Payload: {identifier}. Reply: \
         {ok, identifier}.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.find_by_token",
        |p: Value| async move { ga::handle_find_by_token(p).await },
        "Resolve a `{{token}}` (or bare name) to GLOBAL anchors. Payload: \
         {token}. Reply: {scope, token, anchors, count}.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.find_by_target",
        |p: Value| async move { ga::handle_find_by_target(p).await },
        "Inverse lookup (OI-20): GLOBAL anchors referencing a symbol. Payload: \
         {symbol_id}. Reply: {scope, symbol_id, anchors, count}.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.list_under",
        |p: Value| async move { ga::handle_list_under(p).await },
        "GLOBAL anchors under a taxonomy parent (OI-19). Payload: {parent_id}. \
         Reply: {scope, parent_id, anchors, count}.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
    );
    register_action_with_meta(
        "anchors.promote_via_alias",
        |p: Value| async move { ga::handle_promote_via_alias(p).await },
        "Land an alias-driven promotion in the GLOBAL store (Slice \
         N-data-aliases). Same shape as anchors.create — the WHOLE anchor (all \
         aliases) promotes — with the user-intent audit-logged. Payload: \
         {identifier, kind?, target, description?, aliases?, via_alias?, ...}. \
         Reply: the global Anchor, or already_exists_global on collision.",
        HANDLER_MODULE_GLOBAL_ANCHORS,
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
