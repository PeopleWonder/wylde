//! Central action routing for `wylde-workspaces`.
//!
//! Mirrors the `service::install` pattern every other Rust service uses
//! (`wylde-ollama::service`, `wylde-vram-broker::service`): register the
//! action surface on the process-wide shared registry, then let the shared
//! pipe server dispatch `/__action__` frames into it. Unknown actions get
//! the shared dispatcher's `no_action` reply for free — the same code every
//! service emits — so we don't reinvent routing.
//!
//! Slice 0a registers exactly one verb: [`PING`]. It's a no-op liveness
//! proof that the pipe round-trips through the shared client crate. Every
//! later slice (registry, notes, conversations, anchors, graph) adds its
//! `workspaces.*` verbs to [`install`].

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, unregister_action, Reply};

/// The sole verb in Slice 0a. A no-op that proves the transport works.
pub const PING: &str = "ping";

/// Every action this service registers. Grows one slice at a time.
pub const ALL_ACTIONS: &[&str] = &[PING];

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
        "wylde_workspaces::action_dispatch",
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
