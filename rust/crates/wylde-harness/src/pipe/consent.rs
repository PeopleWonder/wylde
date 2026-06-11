//! `consent.*` pipe registrations (Phase 12.2 + 12.6) -- the per-tool
//! consent gate verbs + the pending-prompt stream. Split from `pipe.rs`
//! per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, StreamSender,
};

use crate::api::HarnessApi;

const HANDLER_MODULE_CONSENT: &str = "wylde_harness::api::DefaultHarnessApi (consent.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── consent.* (Phase 12.2) ───────────────────────────────────────

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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
