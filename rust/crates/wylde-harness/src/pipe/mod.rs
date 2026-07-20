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
//!   [`wylde_shared::ipc::register_action_with_meta`] /
//!   [`wylde_shared::ipc::register_streaming_action_with_meta`]
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
//! * `rag.workspaces.*` (10 verbs) — overlaps `memory.workspaces.*`;
//!   namespace reconciliation + indexer port pending.
//!
//! (`prompts.*` left this list in the full-Rust cutover — the five verbs
//! are served by [`crate::prompts`] now. `memory.workspace.*` followed in
//! the same cutover, slice R2a — the six verbs are served by
//! [`crate::memory::workspace`] now. `memory.reflect` left in slice R2b —
//! all three scopes are served by [`crate::memory::reflection`], which
//! also wires a real in-process chat path the Python verb never had.)
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
//! (`save_conversation` itself, on the chat-turn path) still shares the
//! same JSON files, so the short-term merge-save, the Slice B
//! read/list/delete path, and the R2b reflection rewrite all preserve
//! every sibling field.

//!
//! ## File layout (architecture-review R1)
//!
//! Split into a directory module per architecture-review R1: this file
//! keeps the verb list ([`ALL_PIPE_ACTIONS`]) and the registration
//! spine ([`install_all_against`] / [`install_all`]); the per-domain
//! handler registrations live in the submodules, one per verb family.

use std::sync::Arc;

use crate::api::HarnessApi;

mod chat;
mod consent;
mod conversations;
mod globals;
mod memory;
mod models;
mod prompts;
mod settings;
mod tools;
mod user_profile;

/// Every action the harness pipe registers. Tests compare this against
/// `list_action_meta()` to catch a missing registration. Order mirrors
/// the Phase 9 sectioning so the contract emitter produces stable output.
pub const ALL_PIPE_ACTIONS: &[&str] = &[
    // chat.* — turn driver (7 verbs)
    "chat.run_turn",
    "chat.preview_context",
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
    // chat.* — conversation export/import (TBS Slice J). Standalone served
    // in-process; a payload workspace_id forwards to the workspaces service.
    "chat.export",
    "chat.import",
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
    // settings.encryption.* — encryption-at-rest toggle (OI-14, 2 verbs)
    "settings.encryption.get",
    "settings.encryption.set",
    // settings.concept_routing.* — routing master toggle (concept-routing
    // plan §3, 2 verbs)
    "settings.concept_routing.get",
    "settings.concept_routing.set",
    // settings.reasoning.* + reasoning.fit_check — agentic-reasoning master
    // toggle + model slots + advisory VRAM fit (reasoning plan S1, 3 verbs)
    "settings.reasoning.get",
    "settings.reasoning.set",
    "reasoning.fit_check",
    // prompts.* — system-prompt overrides + presets (5 verbs; Rust port
    // of the Python `_prompts.py` actions, full-Rust cutover)
    "prompts.list",
    "prompts.save",
    "prompts.save_preset",
    "prompts.set_active",
    "prompts.delete_preset",
    // rag.* — RETIRED from the harness pipe (memory plan M7). The
    // tiered RAG store + its `rag.add_episodic` / `rag.search` verbs
    // (Wylde_Study S2a) were retired with the rest of `memory/rag/`.
    // The harness no longer answers them (→ no_action), and WyldeStudy
    // (their only consumer) is de-registered until it returns as an
    // Extension. The vector+graph hybrid path now reads the long-term
    // store via `meta.graph_query` / the `graph` resource.
    // memory.long_term.* — global memory tier (6 verbs)
    "memory.long_term.list",
    "memory.long_term.save",
    "memory.long_term.update",
    "memory.long_term.delete",
    "memory.long_term.history",
    "memory.long_term.search",
    "memory.long_term.reindex",
    // memory.workspace.* — workspace-scoped durable memory tier
    // (8 verbs; R2a base + delete_all (#135) + reindex (#136))
    "memory.workspace.list",
    "memory.workspace.search",
    "memory.workspace.save",
    "memory.workspace.update",
    "memory.workspace.delete",
    "memory.workspace.delete_all",
    "memory.workspace.reindex",
    "memory.workspace.curate",
    // memory.reflect — consolidation cycles, all scopes (full-Rust
    // cutover slice R2b)
    "memory.reflect",
    // workspaces.* — RETIRED from the harness pipe (Thought Bubble System
    // Slice 0d). All workspace verbs now live on the wylde-workspaces
    // service pipe; consumers reach them via the wylde-workspaces-client
    // crate. The harness no longer answers `workspaces.*` (→ no_action).
    // memory.short_term.* — conversation working memory (3 verbs)
    "memory.short_term.get",
    "memory.short_term.append",
    "memory.short_term.clear",
    // conversations.* — lifecycle + active selection + workspace (8 verbs)
    "conversations.new",
    "conversations.list",
    "conversations.get",
    "conversations.delete",
    "conversations.delete_by_workspace",
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
    // ignore.* — GLOBAL symbol ignore list (Slice M, harness tier; the
    // workspace + conversation tiers are `workspaces.ignore.*`). In-process,
    // user-level — same placement rationale as anchors.*.
    "ignore.list",
    "ignore.add",
    "ignore.remove",
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

    chat::install(&api);
    tools::install(&api);
    models::install(&api);
    settings::install(&api);
    prompts::install(&api);
    memory::install(&api);
    conversations::install(&api);
    consent::install(&api);
    user_profile::install(&api);

    // ── anchors.* — GLOBAL anchor store (Slice N-data, harness half) ──────
    //
    // These are file-backed CRUD over `<data_dir>/global_anchors.json`, not a
    // `HarnessApi` method — they have no shared state to thread through the
    // trait, so they register as plain free-fn closures (like the
    // `wylde-workspaces` service's own action handlers).
    globals::install_global_anchor_actions();
    globals::install_global_ignore_actions();
}

/// Backwards-compat shim — equivalent to
/// `install_all_against(DefaultHarnessApi)`. The harness binary uses
/// this from [`crate::service::install`]; callers that want a custom
/// `HarnessApi` (mock for tests, instrumentation wrapper) should call
/// [`install_all_against`] directly.
pub fn install_all() {
    install_all_against(crate::api::DefaultHarnessApi);
}
