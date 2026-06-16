//! `conversations.*` pipe registrations -- lifecycle + active selection
//! + workspace binding. Split from `pipe.rs` per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_CONVERSATIONS: &str =
    "wylde_harness::api::DefaultHarnessApi (conversations.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── conversations.* (conversation lifecycle + active selection) ──

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
    register_action_with_meta(
        "conversations.delete_by_workspace",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_delete_by_workspace(p).await }
        },
        "Sweep every flat-store conversation bound to a workspace (Route 1 \
         deletion complement to the workspaces service-store cascade). \
         Payload {workspace_id}. Returns {ok, workspace_id, deleted} where \
         deleted is the count of swept docs. bad_request for a blank \
         workspace_id (so it can never mass-delete unbound/global chats).",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
    register_action_with_meta(
        "conversations.get_active_for_workspace",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_get_active_for_workspace(p).await }
        },
        "Read the per-workspace last-open conversation pointer (C7), stored in \
         <data_dir>/active_conversation_by_workspace.json, so entering a \
         workspace restores the thread last open in *that* workspace. Payload \
         {workspace_id}. Returns {workspace_id, id} — id is \"\" when none \
         recorded; bad_request for a blank workspace_id (the global \
         get_active owns the unbound surface).",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "conversations.set_active_for_workspace",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.conversations_set_active_for_workspace(p).await }
        },
        "Persist the per-workspace last-open conversation pointer (C7) so \
         re-entering a workspace restores the right thread. Payload \
         {workspace_id, id?}; an empty/absent id clears that workspace's \
         pointer. Returns {workspace_id, id} (the persisted value, \"\" when \
         cleared); bad_request for a blank workspace_id.",
        HANDLER_MODULE_CONVERSATIONS,
    );

    let a = Arc::clone(api);
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
}
