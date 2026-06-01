//! Service-level glue: hold the [`Host`] singleton, register every
//! action handler on the IPC registry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, unregister_action,
};

use crate::actions;
use crate::config::Config;
use crate::host::Host;

pub const ALL_ACTIONS: [&str; 11] = [
    "ext.list",
    "ext.get",
    "ext.enable",
    "ext.disable",
    "ext.tools.list",
    "ext.tools.call",
    "ext.health",
    "ext.restart",
    "extensions.list_panels",
    "ext.events",
    // Back-compat alias kept for the Gateway's existing pipe call
    // shape; deleted after the Gateway switches to ext.tools.call.
    "extensions.dispatch",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);
static HOST: OnceLock<Arc<Host>> = OnceLock::new();

/// Build (or fetch) the process-wide [`Host`].
fn host() -> Arc<Host> {
    HOST.get_or_init(|| Arc::new(Host::new(Config::get()))).clone()
}

/// Register every action handler. Idempotent.
///
/// Note: this does NOT eagerly spawn enabled MCP servers — call
/// [`bootstrap`] after `install()` to run the catalog discovery +
/// spawn loop. (Tests typically skip bootstrap and seed the host
/// directly.)
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let h = host();

    // ── 9 first-class actions ───────────────────────────────────────
    let hc = h.clone();
    register_action_with_meta(
        "ext.list",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_list(hc, payload).await }
        },
        "List installed extensions + their MCP-server status.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.get",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_get(hc, payload).await }
        },
        "{name} — get one extension's manifest + status.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.enable",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_enable(hc, payload).await }
        },
        "{name} — set enabled=true in manifest + spawn its MCP server. Persists.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.disable",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_disable(hc, payload).await }
        },
        "{name} — set enabled=false + SIGTERM. Persists.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.tools.list",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_tools_list(hc, payload).await }
        },
        "[{extension?}] — aggregate tool catalog across enabled extensions, \
         or single-extension catalog if {extension} supplied.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.tools.call",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_tools_call(hc, payload).await }
        },
        "{extension, tool, arguments?} — call a tool on an extension's MCP server.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.health",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_health(hc, payload).await }
        },
        "{extension} — send MCP `ping` to the extension's server.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "ext.restart",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_restart(hc, payload).await }
        },
        "{extension} — stop + start one extension's MCP server.",
        "wylde_extension_bridge::actions::surface",
    );
    let hc = h.clone();
    register_action_with_meta(
        "extensions.list_panels",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::handle_list_panels(hc, payload).await }
        },
        "Union of every enabled extension's `ui_panels`. Pure read; \
         never spawns a server. Consumed by the GUI's Tools tab.",
        "wylde_extension_bridge::actions::surface",
    );

    // ── streaming ───────────────────────────────────────────────────
    let hc = h.clone();
    register_streaming_action_with_meta(
        "ext.events",
        move |payload: Value, sender| {
            let hc = hc.clone();
            async move { actions::handle_events(hc, payload, sender).await; }
        },
        "Stream extension lifecycle events: spawn / exit / restart / crash / enabled / disabled.",
        "wylde_extension_bridge::actions::surface",
    );

    // ── back-compat alias ───────────────────────────────────────────
    let hc = h.clone();
    register_action_with_meta(
        "extensions.dispatch",
        move |payload: Value| {
            let hc = hc.clone();
            async move { actions::legacy_dispatch::handle_extensions_dispatch(hc, payload).await }
        },
        "Back-compat: {extension, endpoint, params} — forwards to ext.tools.call with tool=endpoint. \
         Kept until Gateway switches to ext.tools.call; then removed.",
        "wylde_extension_bridge::actions::legacy_dispatch",
    );

    tracing::info!(
        "wylde-extension-bridge: registered 11 actions (9 native unary + extensions.list_panels + ext.events streaming + extensions.dispatch alias)"
    );
}

/// Bootstrap: load catalog + optionally start enabled extensions.
pub async fn bootstrap() {
    let h = host();
    h.refresh_catalog().await;
    if Config::get().eager_spawn {
        h.start_enabled().await;
    }
}

pub fn stop() {
    // Best-effort: run an async shutdown if we still have a runtime.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let h = host();
        handle.block_on(async move {
            h.shutdown_all().await;
        });
    }
}

#[doc(hidden)]
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
    use wylde_shared::ipc::list_actions;

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
        // The 8 unary actions and the back-compat alias appear in
        // list_actions(); the streaming one doesn't (it's in the
        // streaming registry).
        for n in [
            "ext.list",
            "ext.get",
            "ext.enable",
            "ext.disable",
            "ext.tools.list",
            "ext.tools.call",
            "ext.health",
            "ext.restart",
            "extensions.list_panels",
            "extensions.dispatch",
        ] {
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
}
