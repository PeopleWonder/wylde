//! `memory.*` pipe registrations -- the long_term, workspace, reflect
//! and short_term verb families. Split from `pipe.rs` per
//! architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_LONG_TERM: &str = "wylde_harness::api::DefaultHarnessApi (memory.long_term.*)";
const HANDLER_MODULE_WORKSPACE_MEMORY: &str =
    "wylde_harness::api::DefaultHarnessApi (memory.workspace.*)";
const HANDLER_MODULE_REFLECT: &str = "wylde_harness::api::DefaultHarnessApi (memory.reflect)";
const HANDLER_MODULE_SHORT_TERM: &str =
    "wylde_harness::api::DefaultHarnessApi (memory.short_term.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── memory.long_term.* ───────────────────────────────────────────

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    // ── memory.workspace.* (full-Rust cutover slice R2a) ─────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.workspace.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspace_list(p).await }
        },
        "Return every workspace-scoped record, importance-desc then \
         recency-desc. Payload {workspace_id, include_superseded?}. \
         Returns {memories: [...], count, workspace_id}.",
        HANDLER_MODULE_WORKSPACE_MEMORY,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.workspace.search",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspace_search(p).await }
        },
        "Token-overlap + importance + recency-decay search over a \
         workspace's memories (superseded records filtered out). \
         Payload {workspace_id, query, k?|limit?} — hit count clamps \
         to 1..=50, default 5. Returns {hits: [...]}.",
        HANDLER_MODULE_WORKSPACE_MEMORY,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.workspace.save",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspace_save(p).await }
        },
        "Persist a new workspace-scoped memory. Payload {workspace_id, \
         body, source?, importance?, entities?}. Entity edges mirror \
         into the graph best-effort (never block the save). Returns \
         the new record as JSON.",
        HANDLER_MODULE_WORKSPACE_MEMORY,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.workspace.update",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspace_update(p).await }
        },
        "Revise a workspace memory (writes a new record and marks the \
         old one superseded — both stay on disk for audit walks). \
         Payload {workspace_id, id, body?, importance?, entities?}. \
         Returns the replacement record.",
        HANDLER_MODULE_WORKSPACE_MEMORY,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.workspace.delete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspace_delete(p).await }
        },
        "Permanently remove a workspace memory (and any records \
         superseded by it). Payload {workspace_id, id}. Returns \
         {ok, workspace_id, id}.",
        HANDLER_MODULE_WORKSPACE_MEMORY,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.workspace.curate",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_workspace_curate(p).await }
        },
        "Trigger LLM-driven curation for a workspace. Always returns \
         the skipped CurationResult shape (chat functions don't cross \
         the wire; the scheduler runs real passes in-process). Payload \
         {workspace_id}.",
        HANDLER_MODULE_WORKSPACE_MEMORY,
    );

    // ── memory.reflect (full-Rust cutover slice R2b) ─────────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "memory.reflect",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.memory_reflect(p).await }
        },
        "Run one memory-consolidation cycle. Payload {scope} where scope \
         is \"long_term\" | \"workspace:<id>\" | \"conversation:<id>\". \
         Returns the ReflectionResult dict (scope, inputs_considered, \
         reflection_id, reflection_body, superseded_ids, skipped, \
         skip_reason). Runs a real LLM cycle in-process when a chat model \
         is resolvable; otherwise replies skipped with \"no chat_fn \
         supplied\" (Python parity). The background scheduler drives the \
         same cycles on idle/daily cadences.",
        HANDLER_MODULE_REFLECT,
    );

    // ── workspaces.* — RETIRED (Thought Bubble System Slice 0d) ──────
    // The harness pipe no longer registers any `workspaces.*` verb. All
    // workspace state lives in the wylde-workspaces service; consumers
    // (the chat turn driver, the GUI panels) reach it over its pipe via
    // the wylde-workspaces-client crate. A stray `workspaces.*` call to
    // the harness pipe now returns `no_action`.

    // ── memory.short_term.* (conversation working memory) ────────────

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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
}
