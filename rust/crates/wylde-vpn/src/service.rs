//! Action-registry installer for the WyldeLink VPN service.
//!
//! Mirrors `wylde_ollama::service` and `wylde_vram_broker::service` —
//! registers every action on the process-wide `wylde_shared::ipc`
//! registry, exposes [`install`] / [`stop`] / [`reset_for_tests`].

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{register_action_with_meta, unregister_action};

use crate::actions::{
    all_action_names, contract_metadata, handle_link_config_get, handle_link_config_patch,
    handle_link_connect, handle_link_pair, handle_link_peers, handle_link_peers_remove,
    handle_link_qr, handle_link_register, handle_link_restart, handle_link_status,
    handle_link_stun, handle_vpn_disable, handle_vpn_enable, handle_vpn_keygen, handle_vpn_status,
    handler_module,
};

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `vpn.*` / `link.*` action. Idempotent.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let module = handler_module();
    let metadata: std::collections::HashMap<&str, &str> =
        contract_metadata().into_iter().collect();
    let doc = |name: &str| -> &'static str { metadata.get(name).copied().unwrap_or("") };

    register_action_with_meta(
        "vpn.status",
        |p: Value| async move { handle_vpn_status(p).await },
        doc("vpn.status"),
        module,
    );
    register_action_with_meta(
        "vpn.enable",
        |p: Value| async move { handle_vpn_enable(p).await },
        doc("vpn.enable"),
        module,
    );
    register_action_with_meta(
        "vpn.disable",
        |p: Value| async move { handle_vpn_disable(p).await },
        doc("vpn.disable"),
        module,
    );
    register_action_with_meta(
        "vpn.keygen",
        |p: Value| async move { handle_vpn_keygen(p).await },
        doc("vpn.keygen"),
        module,
    );
    register_action_with_meta(
        "link.status",
        |p: Value| async move { handle_link_status(p).await },
        doc("link.status"),
        module,
    );
    register_action_with_meta(
        "link.pair",
        |p: Value| async move { handle_link_pair(p).await },
        doc("link.pair"),
        module,
    );
    register_action_with_meta(
        "link.register",
        |p: Value| async move { handle_link_register(p).await },
        doc("link.register"),
        module,
    );
    register_action_with_meta(
        "link.stun",
        |p: Value| async move { handle_link_stun(p).await },
        doc("link.stun"),
        module,
    );
    register_action_with_meta(
        "link.peers",
        |p: Value| async move { handle_link_peers(p).await },
        doc("link.peers"),
        module,
    );
    register_action_with_meta(
        "link.peers.remove",
        |p: Value| async move { handle_link_peers_remove(p).await },
        doc("link.peers.remove"),
        module,
    );
    register_action_with_meta(
        "link.connect",
        |p: Value| async move { handle_link_connect(p).await },
        doc("link.connect"),
        module,
    );
    register_action_with_meta(
        "link.qr",
        |p: Value| async move { handle_link_qr(p).await },
        doc("link.qr"),
        module,
    );
    register_action_with_meta(
        "link.config.get",
        |p: Value| async move { handle_link_config_get(p).await },
        doc("link.config.get"),
        module,
    );
    register_action_with_meta(
        "link.config.patch",
        |p: Value| async move { handle_link_config_patch(p).await },
        doc("link.config.patch"),
        module,
    );
    register_action_with_meta(
        "link.restart",
        |p: Value| async move { handle_link_restart(p).await },
        doc("link.restart"),
        module,
    );

    tracing::info!("wylde-vpn: registered {} actions", all_action_names().len());
}

/// Signal stop. Phase 2.B added the tunnel manager — disable both
/// active tunnels (wg0/wg1) on the way out so the wintun adapter is
/// torn down.
pub fn stop() {
    crate::tunnel::state::TunnelManager::get().shutdown_all();
}

/// Test-only: unregister every action and reset the install flag.
pub fn reset_for_tests() {
    for n in all_action_names() {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_actions};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_all_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let actions = list_actions();
        for n in all_action_names() {
            assert!(actions.contains(&n.to_string()), "missing {n}");
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
    async fn dispatch_unknown_returns_no_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "vpn.bogus",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }

    #[tokio::test]
    async fn dispatch_vpn_keygen_round_trips_through_registry() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "vpn.keygen",
            "payload": null,
        }))
        .await;
        assert!(reply.ok);
        assert!(!reply.data["public_key"].as_str().unwrap().is_empty());
        reset_for_tests();
    }
}
