//! `tools.*` pipe registrations -- tool catalog + direct invocation.
//! Split from `pipe.rs` per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_TOOLS: &str = "wylde_harness::api::DefaultHarnessApi (tools.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── tools.* ──────────────────────────────────────────────────────

    let a = Arc::clone(api);
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

    let a = Arc::clone(api);
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
}
