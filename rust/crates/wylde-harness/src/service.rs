//! Service entrypoint: register every harness pipe action on the
//! shared IPC registry.
//!
//! Phase 9 consolidated the verb dispatch table into [`crate::pipe`].
//! This file is now a thin wrapper — it forwards `install()` to
//! [`pipe::install_all`] and exposes the test-only reset.
//!
//! ## Action surface
//!
//! See [`crate::pipe::ALL_PIPE_ACTIONS`] for the full list. The
//! Python `Core/harness/pipe/` modules whose subsystem hasn't been
//! ported yet (rag.workspaces.*) are deliberately
//! NOT registered here —
//! they surface as `no_action` from the IPC dispatcher, which the
//! Python strangler's transport-code fallback treats as a signal to
//! revert to the in-process Python driver. See
//! [`crate::pipe`] module docs for the full deferred list.

use std::sync::atomic::{AtomicBool, Ordering};

use wylde_shared::ipc::unregister_action;

use crate::pipe::{install_all, ALL_PIPE_ACTIONS};

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every pipe action on the process-wide registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    install_all();
    tracing::info!(
        "wylde-harness: registered {} pipe actions",
        ALL_PIPE_ACTIONS.len()
    );
    // Background memory scheduler (full-Rust cutover slice R2b) — the
    // tokio task that fires memory.reflect cycles on idle/daily
    // cadences. Gated on WYLDE_HARNESS_SCHEDULER (default ON); a no-op
    // when no async runtime is present (sync test callers). Its first
    // tick happens one poll interval after boot.
    crate::memory::scheduler::start_default();
    // Warm model slots (agentic-reasoning S2): with the reasoning toggle
    // on, preload the slot models (keep_alive 24h) so the first Deep turn
    // doesn't pay a cold load. Declines when disabled (the default — zero
    // behaviour change) or when no async runtime is present.
    crate::turn::reasoning::residency::spawn_warm_slots("boot");
}

/// Signal stop. The 5.B turn-task pool is detached; outstanding tasks
/// observe the broken pipe to wylde-ollama and the dropped IPC stream
/// receiver and tear themselves down. No explicit join handle here.
pub fn stop() {}

/// Test-only: unregister every action and reset the install flag.
///
/// Deliberately does NOT clear the turn registry — `clear_all_turns()`
/// would race with parallel `turn::actions` tests that just inserted
/// their own (uuid-unique) slot. The action registry and the turn
/// registry are independent; resetting one needn't touch the other.
pub fn reset_for_tests() {
    for n in ALL_PIPE_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_action_meta, list_actions};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_every_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // `list_actions()` upstream only enumerates UNARY handlers
        // (not streaming), so walk the meta map for the full surface
        // — both streaming and unary entries land in `meta`. Pin every
        // name through that surface so we catch a missing register
        // call regardless of variant.
        let meta_names: std::collections::HashSet<String> = list_action_meta()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for n in ALL_PIPE_ACTIONS {
            assert!(meta_names.contains(*n), "missing {n}");
        }
        // The unary-only `list_actions()` is still the right surface
        // for non-streaming actions — pin a known unary subset there.
        // Anything streaming would show up via meta above instead.
        let unary = list_actions();
        for n in [
            "chat.run_turn",
            "chat.preview_context",
            "chat.start_turn",
            "chat.cancel",
            "tools.list",
            "tools.run",
            "memory.long_term.list",
            "memory.long_term.save",
            "memory.long_term.update",
            "memory.long_term.delete",
            "memory.long_term.history",
            // workspaces.* retired from the harness pipe (Slice 0d) — now
            // served by the wylde-workspaces service.
            // memory.short_term.* — conversation working memory
            "memory.short_term.get",
            "memory.short_term.append",
            "memory.short_term.clear",
            // conversations.* — conversation lifecycle + active selection
            "conversations.new",
            "conversations.list",
            "conversations.get",
            "conversations.delete",
            "conversations.get_active",
            "conversations.set_active",
            "conversations.set_workspace",
            // models.* — harness Slice 3a
            "models.list",
            "models.get_profile",
            "models.show",
            "models.delete",
            "models.unload",
            "models.set_active",
            "models.set_default",
            "models.get_default",
            // consent.* — Phase 12.2
            "consent.list",
            "consent.set",
            "consent.respond",
            "consent.clear",
            "consent.set_no_auth",
            "consent.reset",
            // user_profile.* — Thought Bubble System Slice D
            "user_profile.get",
            "user_profile.update",
            "user_profile.propose",
            "user_profile.accept",
            "user_profile.reject",
            "user_profile.list_proposals",
        ] {
            assert!(unary.contains(&n.to_string()), "missing unary {n}");
        }
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

    #[tokio::test]
    async fn dispatching_unknown_subaction_returns_no_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "chat.bogus",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }

    #[tokio::test]
    async fn stream_actions_are_marked_streaming_in_contract_meta() {
        // The contract emitted to data/contracts/actions/<svc>.json
        // carries a `streaming: bool` per action. Pin that the two
        // stream_* actions are marked streaming and the unary chat
        // actions are not, so `wylde_check` parity rules can read it.
        let _g = registry_guard().await;
        reset_for_tests();
        install();

        let meta: std::collections::HashMap<_, _> = list_action_meta().into_iter().collect();
        assert!(!meta["chat.run_turn"].streaming);
        assert!(!meta["chat.start_turn"].streaming);
        assert!(!meta["chat.cancel"].streaming);
        assert!(
            meta["chat.stream_turn"].streaming,
            "chat.stream_turn must be streaming"
        );
        assert!(
            meta["chat.stream_tools"].streaming,
            "chat.stream_tools must be streaming"
        );
        // tools.* + memory.* are unary.
        assert!(!meta["tools.list"].streaming);
        assert!(!meta["tools.run"].streaming);
        assert!(!meta["memory.long_term.list"].streaming);

        reset_for_tests();
    }
}
