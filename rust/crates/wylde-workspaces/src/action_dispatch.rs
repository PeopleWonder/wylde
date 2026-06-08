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

/// Signal stop. No background workers in Slice 0a, so this is a no-op kept
/// symmetric with the other services' `stop()`.
pub fn stop() {}

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
