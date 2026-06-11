//! `prompts.*` pipe registrations -- system-prompt overrides + presets.
//! Split from `pipe.rs` per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_PROMPTS: &str = "wylde_harness::api::DefaultHarnessApi (prompts.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── prompts.* (system-prompt overrides + presets) ────────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "prompts.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.prompts_list(p).await }
        },
        "System-prompt catalog + override store in one envelope: groups, \
         catalog (id/group/label/desc/default), overrides, presets, \
         active_preset. The Settings prompt editor calls this on mount. \
         Payload {}.",
        HANDLER_MODULE_PROMPTS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "prompts.save",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.prompts_save(p).await }
        },
        "Save an override for one prompt id. Payload {id, text?}; \
         text=null (or text equal to the catalog default) clears the \
         override. Returns the full prompts envelope.",
        HANDLER_MODULE_PROMPTS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "prompts.save_preset",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.prompts_save_preset(p).await }
        },
        "Snapshot the current overrides into a named preset and activate \
         it. Payload {name} (\"Default\" is reserved). Returns the full \
         prompts envelope.",
        HANDLER_MODULE_PROMPTS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "prompts.set_active",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.prompts_set_active(p).await }
        },
        "Activate the named preset; \"Default\" resets every override to \
         the catalog default. Payload {name}. Returns the full prompts \
         envelope.",
        HANDLER_MODULE_PROMPTS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "prompts.delete_preset",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.prompts_delete_preset(p).await }
        },
        "Remove a named preset (active falls back to Default if it was \
         the one deleted; \"Default\" itself cannot be deleted). Payload \
         {name}. Returns the full prompts envelope.",
        HANDLER_MODULE_PROMPTS,
    );
}
