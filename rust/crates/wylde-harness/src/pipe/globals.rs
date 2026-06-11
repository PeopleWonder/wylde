//! GLOBAL anchor + ignore store registrations (`anchors.*`, `ignore.*`)
//! -- plain free-fn closures over the file-backed stores, not
//! `HarnessApi` methods. Split from `pipe.rs` per architecture-review
//! R1.

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

const HANDLER_MODULE_GLOBAL_ANCHORS: &str = "wylde_harness::global_anchors::api (anchors.*)";
const HANDLER_MODULE_GLOBAL_IGNORE: &str = "wylde_harness::chat::ignore (ignore.*)";

/// Register the three in-process `ignore.*` (global-tier) verbs (Slice M).
pub(super) fn install_global_ignore_actions() {
    use crate::chat::ignore as gi;

    register_action_with_meta(
        "ignore.list",
        |p: Value| async move { gi::handle_list(p).await },
        "Every GLOBALLY ignored composer token (Plan §5.8: ignored = \
         default-inactive, still highlights). No payload. Reply: {scope: \
         \"global\", ignored: [{token, added_at}], count}. Workspace + \
         conversation tiers live on workspaces.ignore.*.",
        HANDLER_MODULE_GLOBAL_IGNORE,
    );
    register_action_with_meta(
        "ignore.add",
        |p: Value| async move { gi::handle_add(p).await },
        "Ignore a token globally. Payload: {token}. Reply: {ok, added, \
         token} — re-adding succeeds with added=false (idempotent).",
        HANDLER_MODULE_GLOBAL_IGNORE,
    );
    register_action_with_meta(
        "ignore.remove",
        |p: Value| async move { gi::handle_remove(p).await },
        "Stop ignoring a token globally. Payload: {token}. Reply: {ok, \
         removed, token}.",
        HANDLER_MODULE_GLOBAL_IGNORE,
    );
}

/// Register the eight in-process `anchors.*` (global-scope) verbs. Split out so
/// `install_all_against` stays readable; called at the end of it.
pub(super) fn install_global_anchor_actions() {
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
