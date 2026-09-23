//! Service entrypoint: register the 8 `n8n.*` actions on the shared
//! IPC registry. Same shape as `wylde-ollama::service`.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{register_action_with_meta, unregister_action};

use crate::actions;

const ALL_ACTIONS: [&str; 8] = [
    "n8n.health",
    "n8n.list_workflows",
    "n8n.get_workflow",
    "n8n.get_execution",
    "n8n.execute_workflow",
    "n8n.create_workflow",
    "n8n.edit_workflow",
    "n8n.delete_workflow",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `n8n.*` action on the process-wide registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    register_action_with_meta(
        "n8n.health",
        |payload: Value| async move { actions::handle_health(payload, actions::client()).await },
        "Service liveness + upstream probe. Reply: {auth_configured, url, \
         reachable} — reachable is a quick unauthenticated GET against the \
         external n8n daemon, fail-soft false.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.list_workflows",
        |payload: Value| async move { actions::handle_list_workflows(payload, actions::client()).await },
        "GET /rest/workflows — reply {workflows: [{id, name, active, \
         description}], count}.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.get_workflow",
        |payload: Value| async move { actions::handle_get_workflow(payload, actions::client()).await },
        "GET /rest/workflows/{workflow_id} — reply {workflow}. 404 → \
         {error, code: not_found}.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.get_execution",
        |payload: Value| async move { actions::handle_get_execution(payload, actions::client()).await },
        "GET /rest/executions/{execution_id} — reply {execution}. 404 → \
         {error, code: not_found}.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.execute_workflow",
        |payload: Value| async move {
            actions::handle_execute_workflow(payload, actions::client()).await
        },
        "POST /rest/workflows/{workflow_id}/run {data: inputs} — reply \
         {execution_id, status, data}. workflow_id must be a numeric string.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.create_workflow",
        |payload: Value| async move {
            actions::handle_create_workflow(payload, actions::client()).await
        },
        "POST /rest/workflows {name, nodes?, connections?, active?, settings?} \
         — reply {workflow_id, name, active, created_at}. name is required.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.edit_workflow",
        |payload: Value| async move { actions::handle_edit_workflow(payload, actions::client()).await },
        "PATCH /rest/workflows/{workflow_id} — only provided keys of \
         name/nodes/connections/active are sent ('No updatable fields \
         provided' guard). Reply {workflow_id, name, active, updated_at}.",
        "wylde_n8n::actions",
    );
    register_action_with_meta(
        "n8n.delete_workflow",
        |payload: Value| async move {
            actions::handle_delete_workflow(payload, actions::client()).await
        },
        "Archive-then-delete sequence (POST …/archive, then DELETE) — n8n \
         requires archiving first. Reply {deleted: true, workflow_id}.",
        "wylde_n8n::actions",
    );

    tracing::info!("wylde-n8n: registered {} actions", ALL_ACTIONS.len());
}

/// Signal stop. Currently a no-op — the service has no background
/// workers beyond the per-request handlers. Kept symmetric with the
/// other service crates' API.
pub fn stop() {}

/// Test-only: unregister every action and reset the install flag.
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
    use wylde_shared::ipc::{assert_action_table_matches_registry, dispatch_action};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_all_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // #130: both directions — a registered n8n.* verb missing from
        // ALL_ACTIONS now fails, not only a listed-but-unregistered one.
        assert_action_table_matches_registry(&["n8n."], &ALL_ACTIONS);
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        install();
        // Don't blow up. Reset to keep the registry clean for siblings.
        reset_for_tests();
    }

    #[tokio::test]
    async fn dispatching_unknown_subaction_returns_no_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "n8n.bogus",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }
}
