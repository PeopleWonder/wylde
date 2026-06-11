//! `user_profile.*` pipe registrations (TBS Slice D) -- global
//! user-level facts + LLM-proposed updates. Split from `pipe.rs` per
//! architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_USER_PROFILE: &str = "wylde_harness::api::DefaultHarnessApi (user_profile.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── user_profile.* (Thought Bubble System Slice D) ───────────────
    // In-process harness verbs; the profile is read into every turn and
    // edited from the Settings "Profile / Rules" page.

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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
}
