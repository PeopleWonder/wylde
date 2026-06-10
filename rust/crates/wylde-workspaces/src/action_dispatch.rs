//! Central action routing for `wylde-workspaces`.
//!
//! Mirrors the `service::install` pattern every other Rust service uses
//! (`wylde-ollama::service`, `wylde-vram-broker::service`): register the
//! action surface on the process-wide shared registry, then let the shared
//! pipe server dispatch `/__action__` frames into it. Unknown actions get
//! the shared dispatcher's `no_action` reply for free — the same code every
//! service emits — so we don't reinvent routing.
//!
//! Slice 0a registered exactly one verb: [`PING`]. Slice 0b adds the
//! relocated workspace verb surface ([`crate::api`]) — registry CRUD +
//! active-selection, persona write, and the RAG query / reindex verbs — so
//! the new pipe natively serves everything the harness pipe used to. The
//! harness keeps the same verbs as a thin proxy (compat shim) during the
//! migration window; both pipes answer the same `workspaces.*` names.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, unregister_action, Reply};

use crate::api;

const META_MODULE: &str = "wylde_workspaces::action_dispatch";

/// A no-op verb that proves the transport works.
pub const PING: &str = "ping";

// ── Relocated workspace verbs (Slice 0b) ─────────────────────────────────
pub const SET_ACTIVE: &str = "workspaces.set_active";
pub const CREATE: &str = "workspaces.create";
pub const UPDATE: &str = "workspaces.update";
pub const DELETE: &str = "workspaces.delete";
pub const SET_PERSONA: &str = "workspaces.set_persona";
pub const LIST_MRU: &str = "workspaces.list_mru";
pub const RAG_QUERY: &str = "workspaces.rag_query";
pub const REINDEX: &str = "workspaces.reindex";

// ── Chat-turn prompt context (Slice 0d — relocated from the harness) ─────
pub const GATHER_PROMPT: &str = "workspaces.gather_prompt";

// ── Code graph read API (Slice B — Phase 1) ──────────────────────────────
pub const GRAPH: &str = "workspaces.graph";

// ── Symbol index read API (Slice F-data — Phase 1) ───────────────────────
pub const SYMBOLS_FIND: &str = "workspaces.symbols.find";

// ── Symbol context read API (Slice G-data — Phase 1) ─────────────────────
pub const SYMBOL_CONTEXT: &str = "workspaces.symbol_context";

// ── Workspace notes tier (Slice 0c) ──────────────────────────────────────
pub const NOTES_LIST: &str = "workspaces.notes.list";
pub const NOTES_ADD: &str = "workspaces.notes.add";
pub const NOTES_UPDATE: &str = "workspaces.notes.update";
pub const NOTES_DELETE: &str = "workspaces.notes.delete";
pub const NOTES_SEARCH: &str = "workspaces.notes.search";
pub const NOTES_PROPOSE: &str = "workspaces.notes.propose";

// ── Workspace-scoped conversations (Slice 0c) ────────────────────────────
pub const CONVERSATIONS_LIST: &str = "workspaces.conversations.list";
pub const CONVERSATIONS_GET: &str = "workspaces.conversations.get";
pub const CONVERSATIONS_DELETE: &str = "workspaces.conversations.delete";
pub const CONVERSATIONS_REFRESH_SUMMARY: &str = "workspaces.conversations.refresh_summary";
// Slice J escape hatch — `chat.*` names per Plan v2 Appendix A (the
// conversations api owns chat.{export,import} on THIS service's pipe).
pub const CHAT_EXPORT: &str = "chat.export";
pub const CHAT_IMPORT: &str = "chat.import";

// ── File watcher control (Slice I) ───────────────────────────────────────
pub const WATCHER_STATUS: &str = "workspaces.watcher.status";
pub const WATCHER_PAUSE: &str = "workspaces.watcher.pause";
pub const WATCHER_RESUME: &str = "workspaces.watcher.resume";

// ── Workspace anchor store (Slice N-data — Phase 2) ──────────────────────
pub const ANCHORS_LIST: &str = "workspaces.anchors.list";
pub const ANCHORS_CREATE: &str = "workspaces.anchors.create";
pub const ANCHORS_UPDATE: &str = "workspaces.anchors.update";
pub const ANCHORS_DELETE: &str = "workspaces.anchors.delete";
pub const ANCHORS_FIND_BY_TOKEN: &str = "workspaces.anchors.find_by_token";
pub const ANCHORS_FIND_BY_TARGET: &str = "workspaces.anchors.find_by_target";
pub const ANCHORS_LIST_UNDER: &str = "workspaces.anchors.list_under";
pub const ANCHORS_PROPOSE: &str = "workspaces.anchors.propose";
pub const ANCHORS_PROMOTE_VIA_ALIAS: &str = "workspaces.anchors.promote_via_alias";

// ── Symbol ignore list — workspace + conversation tiers (Slice M) ────────
pub const IGNORE_LIST: &str = "workspaces.ignore.list";
pub const IGNORE_ADD: &str = "workspaces.ignore.add";
pub const IGNORE_REMOVE: &str = "workspaces.ignore.remove";

// ── LLM anchor proposals — the review queue (Slice N) ────────────────────
pub const ANCHORS_LIST_PROPOSALS: &str = "workspaces.anchors.list_proposals";
pub const ANCHORS_ACCEPT_PROPOSAL: &str = "workspaces.anchors.accept_proposal";
pub const ANCHORS_REJECT_PROPOSAL: &str = "workspaces.anchors.reject_proposal";

/// Every action this service registers. Grows one slice at a time.
pub const ALL_ACTIONS: &[&str] = &[
    PING,
    SET_ACTIVE,
    CREATE,
    UPDATE,
    DELETE,
    SET_PERSONA,
    LIST_MRU,
    RAG_QUERY,
    REINDEX,
    // Slice 0d — chat-turn prompt context
    GATHER_PROMPT,
    // Slice B — code graph read API
    GRAPH,
    // Slice F-data — symbol index read API
    SYMBOLS_FIND,
    // Slice G-data — symbol context read API
    SYMBOL_CONTEXT,
    // Slice 0c — notes
    NOTES_LIST,
    NOTES_ADD,
    NOTES_UPDATE,
    NOTES_DELETE,
    NOTES_SEARCH,
    NOTES_PROPOSE,
    // Slice 0c — workspace conversations
    CONVERSATIONS_LIST,
    CONVERSATIONS_GET,
    CONVERSATIONS_DELETE,
    // Slice I — file watcher control
    WATCHER_STATUS,
    WATCHER_PAUSE,
    WATCHER_RESUME,
    // Slice N-data — workspace anchor store
    ANCHORS_LIST,
    ANCHORS_CREATE,
    ANCHORS_UPDATE,
    ANCHORS_DELETE,
    ANCHORS_FIND_BY_TOKEN,
    ANCHORS_FIND_BY_TARGET,
    ANCHORS_LIST_UNDER,
    ANCHORS_PROPOSE,
    ANCHORS_PROMOTE_VIA_ALIAS,
    // Slice M — symbol ignore list (workspace + conversation tiers)
    IGNORE_LIST,
    IGNORE_ADD,
    IGNORE_REMOVE,
    // Slice N — LLM anchor-proposal review queue
    ANCHORS_LIST_PROPOSALS,
    ANCHORS_ACCEPT_PROPOSAL,
    ANCHORS_REJECT_PROPOSAL,
    // Slice J — conversation export / import (the escape hatch)
    CHAT_EXPORT,
    CHAT_IMPORT,
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `wylde-workspaces` action on the shared registry.
/// Idempotent — repeat calls are no-ops, matching the broker/ollama shape.
///
/// Must run before `serve()` so the registry is populated when the first
/// pipe client connects.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    register_action_with_meta(
        PING,
        |_payload: Value| async move { handle_ping() },
        "Liveness proof. Reply: {ok: true, service: \"wylde-workspaces\", version: <crate version>}.",
        META_MODULE,
    );

    register_action_with_meta(
        SET_ACTIVE,
        |p: Value| async move { api::handle_set_active(p).await },
        "Set the active workspace + bump MRU. Payload: {workspace_id}. \
         Reply: {active_id, mru}.",
        META_MODULE,
    );
    register_action_with_meta(
        CREATE,
        |p: Value| async move { api::handle_create(p).await },
        "Register a folder as a workspace (and activate it). Payload: \
         {folder, name?}. Reply: WorkspaceDefinition.",
        META_MODULE,
    );
    register_action_with_meta(
        UPDATE,
        |p: Value| async move { api::handle_update(p).await },
        "Rename / toggle persona_enabled / rag_enabled. Payload: \
         {workspace_id, name?, persona_enabled?, rag_enabled?}. Reply: \
         WorkspaceDefinition.",
        META_MODULE,
    );
    register_action_with_meta(
        DELETE,
        |p: Value| async move { api::handle_delete(p).await },
        "Remove a workspace + its data dir. Payload: {workspace_id}. \
         Reply: {ok, workspace_id}.",
        META_MODULE,
    );
    register_action_with_meta(
        SET_PERSONA,
        |p: Value| async move { api::handle_set_persona(p).await },
        "Write persona.md for a workspace. Payload: {workspace_id, text?}. \
         Reply: {ok, workspace_id}.",
        META_MODULE,
    );
    register_action_with_meta(
        LIST_MRU,
        |p: Value| async move { api::handle_list_mru(p).await },
        "MRU-5 workspace list + active id. No payload. Reply: \
         {workspaces: [WorkspaceDefinition], active_id}.",
        META_MODULE,
    );
    register_action_with_meta(
        RAG_QUERY,
        |p: Value| async move { api::handle_rag_query(p).await },
        "k-NN search over a workspace's file index. Payload: \
         {workspace_id, query, k?}. Reply: {workspace_id, hits}. Fail-soft \
         to empty hits.",
        META_MODULE,
    );
    register_action_with_meta(
        REINDEX,
        |p: Value| async move { api::handle_reindex(p).await },
        "Force a synchronous full reindex of a workspace's folder. Payload: \
         {workspace_id}. Reply: {ok, file_count, chunk_count, last_error}.",
        META_MODULE,
    );

    register_action_with_meta(
        GATHER_PROMPT,
        |p: Value| async move { api::handle_gather_prompt(p).await },
        "Resolve a workspace's contribution to a chat turn's system prompt \
         (persona + notes + RAG). Payload: {workspace_id, user_message?}. \
         Reply: {workspace_id, slots, persona, memory_snippets, \
         rag_snippets}. `slots` is the ready-to-append rendered block; \
         empty for an unknown/blank workspace.",
        META_MODULE,
    );

    // ── Slice B — code graph read API ────────────────────────────────────
    register_action_with_meta(
        GRAPH,
        |p: Value| async move { crate::graph::api::handle_graph(p).await },
        "The active workspace's code graph, read live from Neo4j. Payload: \
         {workspace_id}. Reply: WorkspaceGraph {nodes, edges, clusters}. \
         Read-only; idempotent. Empty graph for an unknown/empty workspace; \
         bolt_* codes when the graph backend is unreachable.",
        META_MODULE,
    );

    // ── Slice F-data — symbol index read API ─────────────────────────────
    register_action_with_meta(
        SYMBOLS_FIND,
        |p: Value| async move { crate::graph::symbol_index::handle_symbols_find(p).await },
        "Resolve a name to workspace symbols (exact-first, fuzzy-fill). \
         Payload: {workspace_id, query, limit?}. Reply: {query, matches:[{entry, \
         score}]}. Served from the active workspace's in-memory index \
         (microsecond exact / <50ms fuzzy); on-demand build fallback when the \
         index isn't warm. limit defaults to 20.",
        META_MODULE,
    );

    // ── Slice G-data — symbol context read API ───────────────────────────
    register_action_with_meta(
        SYMBOL_CONTEXT,
        |p: Value| async move { crate::graph::neighborhood::handle_symbol_context(p).await },
        "Structural context for one symbol: body + callers + callees + types \
         used + file siblings, read live from Neo4j. Payload: {workspace_id, \
         symbol_id, hops?=1, include_body?=true, include_blame?=true}. Reply: \
         SymbolContext {symbol, callers, callees, types_used, siblings, \
         hops_traversed, took_ms}; symbol.blame carries recent per-commit git \
         blame for the focal's body lines (Slice L — tracked files only, \
         fail-soft absent otherwise). Read-only; idempotent. `hops` walks the \
         call graph (per-hop time budget 200ms+300ms×N); not_found when the \
         symbol isn't in the workspace; bolt_* codes when the backend is \
         unreachable.",
        META_MODULE,
    );

    // ── Slice 0c — workspace notes tier ──────────────────────────────────
    register_action_with_meta(
        NOTES_LIST,
        |p: Value| async move { crate::notes::api::handle_list(p).await },
        "Every note for a workspace. Payload: {workspace_id}. Reply: \
         {workspace_id, notes, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_ADD,
        |p: Value| async move { crate::notes::api::handle_add(p).await },
        "Append a workspace note (embeds on write). Payload: {workspace_id, \
         text}. Reply: the new note {id, text, created_at, last_used_at}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_UPDATE,
        |p: Value| async move { crate::notes::api::handle_update(p).await },
        "Edit a note's text (re-embeds). Payload: {workspace_id, id, text}. \
         Reply: the updated note. not_found for an unknown id.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_DELETE,
        |p: Value| async move { crate::notes::api::handle_delete(p).await },
        "Remove a note by id. Payload: {workspace_id, id}. Reply: {ok, id}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_SEARCH,
        |p: Value| async move { crate::notes::api::handle_search(p).await },
        "Recency+relevance ranked search over a workspace's notes. Payload: \
         {workspace_id, query, limit?}. Fail-soft to empty. Reply: \
         {workspace_id, notes, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_PROPOSE,
        |p: Value| async move { crate::notes::api::handle_propose(p).await },
        "Reflection candidate note (NOT persisted; user accepts via \
         notes.add). Payload: {workspace_id, text}. Reply: {candidate} or \
         {candidate: null} for blank text.",
        META_MODULE,
    );

    // ── Slice 0c — workspace-scoped conversations ────────────────────────
    register_action_with_meta(
        CONVERSATIONS_LIST,
        |p: Value| async move { crate::conversations::api::handle_list(p).await },
        "Metadata for one workspace's conversations, newest-first. Payload: \
         {workspace_id}. Reply: {workspace_id, conversations, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONVERSATIONS_GET,
        |p: Value| async move { crate::conversations::api::handle_get(p).await },
        "Full conversation document. Payload: {workspace_id, id}. \
         bad_request for a missing/invalid id, not_found when absent.",
        META_MODULE,
    );
    register_action_with_meta(
        CONVERSATIONS_DELETE,
        |p: Value| async move { crate::conversations::api::handle_delete(p).await },
        "Remove one workspace conversation. Payload: {workspace_id, id}. \
         Reply: {ok, id}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONVERSATIONS_REFRESH_SUMMARY,
        |p: Value| async move { crate::conversations::api::handle_refresh_summary(p).await },
        "Persist an LLM summary + embedding for a workspace conversation \
         (Slice E parity; harness generates, service stores). Payload: \
         {workspace_id, conversation_id, summary, embedding, topic_tags?, \
         summary_msg_count?}. Reply: {ok, id}. not_found when absent.",
        META_MODULE,
    );

    // ── Slice J — conversation export / import (the escape hatch) ────────
    register_action_with_meta(
        CHAT_EXPORT,
        |p: Value| async move { crate::conversations::api::handle_export(p).await },
        "Export one workspace conversation as a portable plaintext envelope \
         (wylde-conversation-export v1). Payload: {workspace_id, \
         conversation_id}. Reply: {export, id}. The caller persists the file.",
        META_MODULE,
    );
    register_action_with_meta(
        CHAT_IMPORT,
        |p: Value| async move { crate::conversations::api::handle_import(p).await },
        "Import a portable conversation envelope into a workspace. Payload: \
         {workspace_id, export, overwrite?}. Reply: {imported, workspace_id}. \
         already_exists on an id collision unless overwrite:true.",
        META_MODULE,
    );

    // ── Slice I — file watcher control ───────────────────────────────────
    register_action_with_meta(
        WATCHER_STATUS,
        |p: Value| async move { api::handle_watcher_status(p).await },
        "File-watcher status for observability. No payload. Reply: \
         {active_workspace, files_watched, last_event_at, paused}.",
        META_MODULE,
    );
    register_action_with_meta(
        WATCHER_PAUSE,
        |p: Value| async move { api::handle_watcher_pause(p).await },
        "Pause the active workspace's file watcher (e.g. before a big \
         checkout). No payload. Reply: {ok, paused: true, active_workspace}.",
        META_MODULE,
    );
    register_action_with_meta(
        WATCHER_RESUME,
        |p: Value| async move { api::handle_watcher_resume(p).await },
        "Resume the file watcher and re-walk the workspace to catch up on \
         edits missed while paused. No payload. Reply: {ok, paused: false, \
         active_workspace}.",
        META_MODULE,
    );

    // ── Slice N-data — workspace anchor store ────────────────────────────
    register_action_with_meta(
        ANCHORS_LIST,
        |p: Value| async move { crate::anchors::api::handle_list(p).await },
        "Every anchor for a workspace. Payload: {workspace_id}. Reply: \
         {workspace_id, anchors, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_CREATE,
        |p: Value| async move { crate::anchors::api::handle_create(p).await },
        "Mint a workspace anchor. Payload: {workspace_id, identifier, kind?, \
         target, description?, parent_anchor?, domain?, related_to?}. Reply: \
         the Anchor. `already_exists` (details carry the existing definition) \
         on a duplicate identifier; `bad_request` on a bad identifier/target.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_UPDATE,
        |p: Value| async move { crate::anchors::api::handle_update(p).await },
        "Patch an anchor's description/target/related_to/parent_anchor/domain. \
         Payload: {workspace_id, identifier, ...patch}. Reply: the updated \
         Anchor. not_found for an unknown identifier.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_DELETE,
        |p: Value| async move { crate::anchors::api::handle_delete(p).await },
        "Remove an anchor by identifier. Payload: {workspace_id, identifier}. \
         Reply: {ok, identifier}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_FIND_BY_TOKEN,
        |p: Value| async move { crate::anchors::api::handle_find_by_token(p).await },
        "Resolve a `{{token}}` (or bare name) to a workspace's anchors — \
         composer recognition. Payload: {workspace_id, token}. Reply: \
         {workspace_id, token, anchors, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_FIND_BY_TARGET,
        |p: Value| async move { crate::anchors::api::handle_find_by_target(p).await },
        "Inverse lookup (OI-20): every anchor referencing a symbol. Payload: \
         {workspace_id, symbol_id}. Reply: {workspace_id, symbol_id, anchors, \
         count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_LIST_UNDER,
        |p: Value| async move { crate::anchors::api::handle_list_under(p).await },
        "Anchors under a taxonomy parent (OI-19 hierarchy). Payload: \
         {workspace_id, parent_id}. Reply: {workspace_id, parent_id, anchors, \
         count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_PROPOSE,
        |p: Value| async move { crate::anchors::api::handle_propose(p).await },
        "LLM reflection candidate anchor (NOT persisted; user accepts via \
         anchors.create). Applies OI-7 spam control from caller-supplied \
         counters. Payload: {workspace_id, identifier, target, kind?, \
         description?, confidence?, rationale?, proposals_so_far?, \
         last_proposal_at?}. Reply: {candidate} or {candidate: null, reason}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_PROMOTE_VIA_ALIAS,
        |p: Value| async move { crate::anchors::api::handle_promote_via_alias(p).await },
        "Promote an anchor to global because the user acted on one of its \
         aliases — the WHOLE anchor (all aliases) promotes. Validates the alias \
         belongs to the anchor + audit-logs the intent, then returns the \
         promotion payload for the caller to land via the global \
         anchors.promote_via_alias. Payload: {workspace_id, anchor_id, alias}. \
         Reply: {anchor, via_alias, promote: true}.",
        META_MODULE,
    );

    // ── Slice M — symbol ignore list (workspace + conversation tiers) ────
    register_action_with_meta(
        IGNORE_LIST,
        |p: Value| async move { crate::ignore::api::handle_list(p).await },
        "Both service-side ignore tiers for a workspace. Payload: \
         {workspace_id, conversation_id?}. Reply: {workspace_id, workspace: \
         [{token, added_at}], conversation: [...], conversation_id}. The \
         global tier lives in the harness.",
        META_MODULE,
    );
    register_action_with_meta(
        IGNORE_ADD,
        |p: Value| async move { crate::ignore::api::handle_add(p).await },
        "Ignore a token in one tier (default-inactive from now on, Plan \
         §5.8). Payload: {workspace_id, tier: workspace|conversation, token, \
         conversation_id? (required for conversation)}. Reply: {ok, added, \
         workspace_id, token} — re-adding succeeds with added=false \
         (idempotent write).",
        META_MODULE,
    );
    register_action_with_meta(
        IGNORE_REMOVE,
        |p: Value| async move { crate::ignore::api::handle_remove(p).await },
        "Stop ignoring a token in one tier. Payload: {workspace_id, tier, \
         token, conversation_id?}. Reply: {ok, removed, workspace_id, token}.",
        META_MODULE,
    );

    // ── Slice N — LLM anchor-proposal review queue ───────────────────────
    register_action_with_meta(
        ANCHORS_LIST_PROPOSALS,
        |p: Value| async move { crate::anchors::api::handle_list_proposals(p).await },
        "Pending LLM anchor proposals awaiting user review (user-accept-\
         always, OI-18). Payload: {workspace_id}. Reply: {workspace_id, \
         proposals: [{anchor, confidence, rationale, proposed_at}], count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_ACCEPT_PROPOSAL,
        |p: Value| async move { crate::anchors::api::handle_accept_proposal(p).await },
        "Land a pending proposal in the anchor store. Payload: {workspace_id, \
         identifier, merge?}. Reply: {accepted: created|merged, anchor}. A \
         colliding identifier returns already_exists with {existing, proposal} \
         details (the OI-18 diff view) and keeps the proposal pending; \
         merge:true applies the proposal onto the existing record instead.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_REJECT_PROPOSAL,
        |p: Value| async move { crate::anchors::api::handle_reject_proposal(p).await },
        "Dismiss a pending proposal + record the OI-11 suppression (30 days \
         default; WYLDE_ANCHOR_REJECTION_SUPPRESS_DAYS). Payload: \
         {workspace_id, identifier}. Reply: {ok, rejected, identifier}.",
        META_MODULE,
    );

    tracing::info!(
        "wylde-workspaces: registered {} action(s)",
        ALL_ACTIONS.len()
    );
}

/// Handle the `ping` verb. Pure — no I/O — so it doubles as the unit under
/// test for the reply shape the integration test asserts over the wire.
pub fn handle_ping() -> Reply {
    Reply::ok(json!({
        "ok": true,
        "service": "wylde-workspaces",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Signal stop. Tears down the Slice I file watcher (drops its OS handle +
/// ends its background loop); idempotent if none is running.
pub fn stop() {
    crate::watcher::stop();
    crate::graph::symbol_index::stop();
}

/// Test-only: unregister every action and reset the install flag so a test
/// can re-`install()` on the shared (process-wide) registry cleanly.
pub fn reset_for_tests() {
    for n in ALL_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_actions};

    // The action registry is process-wide; serialize the tests that
    // install/reset it so parallel threads don't clobber each other's
    // registration. Same guard pattern as `wylde-ollama::service::tests`.
    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[test]
    fn ping_reply_shape() {
        // Pure — does not touch the registry, so no guard needed.
        let reply = handle_ping();
        assert!(reply.ok);
        assert_eq!(reply.data["ok"], json!(true));
        assert_eq!(reply.data["service"], "wylde-workspaces");
        assert_eq!(reply.data["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn install_registers_ping_and_dispatches() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        assert!(list_actions().contains(&PING.to_string()));

        let reply = dispatch_action(json!({"action": PING, "payload": null})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["service"], "wylde-workspaces");
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_registers_symbol_context_and_validates_payload() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // The Slice G-data verb is registered and dispatchable.
        assert!(list_actions().contains(&SYMBOL_CONTEXT.to_string()));

        // A blank symbol_id is rejected before any Bolt connection — proves
        // the verb is wired through real dispatch without needing Neo4j.
        let reply = dispatch_action(json!({
            "action": SYMBOL_CONTEXT,
            "payload": { "workspace_id": "ws", "symbol_id": "  " }
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(json!({"action": "workspaces.bogus", "payload": null})).await;
        assert!(!reply.ok);
        // Shared dispatcher's stable code for an unregistered action.
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        install();
        reset_for_tests();
    }
}
