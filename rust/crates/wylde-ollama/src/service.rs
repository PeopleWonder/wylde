//! Service entrypoint: register the 10 `ollama.*` actions on the shared
//! IPC registry. Same shape as `wylde-vram-broker::service`.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, unregister_action,
};

use crate::actions::{chat, embed, gc, models, pull};
use crate::upstream;

const ALL_ACTIONS: [&str; 14] = [
    "ollama.health",
    "ollama.list_models",
    "ollama.list_loaded",
    "ollama.show",
    "ollama.get_model_defaults",
    "ollama.delete",
    "ollama.eject",
    "ollama.preload",
    "ollama.pull",
    "ollama.chat",
    "ollama.chat_stream",
    "ollama.embed",
    "ollama.gc",
    "ollama.store_usage",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `ollama.*` action on the process-wide registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    // ── Unary actions (9) ────────────────────────────────────────────
    register_action_with_meta(
        "ollama.health",
        |payload: Value| async move { models::handle_health(payload, upstream::client()).await },
        "Wrapper liveness + upstream probe (GET /api/tags). Reply: \
         {ok: true, pong: true, upstream: \"ok\"|\"unreachable\"|\"timeout\", upstream_models?}.",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.list_models",
        |payload: Value| async move { models::handle_list_models(payload, upstream::client()).await },
        "GET /api/tags — full installed-model list (passthrough envelope).",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.list_loaded",
        |payload: Value| async move { models::handle_list_loaded(payload, upstream::client()).await },
        "GET /api/ps — currently-loaded models with VRAM/expires_at.",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.show",
        |payload: Value| async move { models::handle_show(payload, upstream::client()).await },
        "POST /api/show {model} — model metadata (details/model_info/parameters).",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.get_model_defaults",
        |payload: Value| async move {
            models::handle_get_model_defaults(payload, upstream::client()).await
        },
        "POST /api/show {model} → sparse model-declared inference defaults \
         parsed from the `parameters` blob (only keys the model sets, e.g. \
         temperature/top_p). 404 → model_not_found; transport → ollama_unreachable.",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.delete",
        |payload: Value| async move { models::handle_delete(payload, upstream::client()).await },
        "DELETE /api/delete {name|model} — drop a local model. 404 → model_not_found.",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.eject",
        |payload: Value| async move { models::handle_eject(payload, upstream::client()).await },
        "POST /api/generate {model, keep_alive:0} — evict a model from VRAM.",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.preload",
        |payload: Value| async move { models::handle_preload(payload, upstream::client()).await },
        "POST /api/generate {model, prompt:'', keep_alive:'24h'} — load a model \
         into VRAM without generating tokens. Caller may pass `keep_alive`.",
        "wylde_ollama::actions::models",
    );
    register_action_with_meta(
        "ollama.embed",
        |payload: Value| async move { embed::handle_embed(payload, upstream::client()).await },
        "POST /api/embed — embed text. Acquires a VRAM lease unless WYLDE_OLLAMA_EMBED_SKIP_BROKER=1.",
        "wylde_ollama::actions::embed",
    );
    register_action_with_meta(
        "ollama.gc",
        |payload: Value| async move { gc::handle_gc(payload, upstream::client()).await },
        "Keep-only-referenced model-store reclaim (#100). Payload: \
         {keep:[tags] (required, protected), pins?:[tags], superseded?:[tags] \
         (present ⇒ only these eligible; absent ⇒ sweep all unreferenced), \
         dry_run?:bool (default true)}. Referenced/pinned models are NEVER \
         reclaimed. Reply: {dry_run, mode, total_bytes, keep, reclaim, \
         reclaimable_bytes, deleted, freed_bytes, errors}.",
        "wylde_ollama::actions::gc",
    );
    register_action_with_meta(
        "ollama.store_usage",
        |payload: Value| async move { gc::handle_store_usage(payload, upstream::client()).await },
        "GET /api/tags → model-store total size + per-model sizes \
         (largest-first). Reply: {total_bytes, model_count, models:[{name,size}]}.",
        "wylde_ollama::actions::gc",
    );
    register_action_with_meta(
        "ollama.chat",
        |payload: Value| async move { chat::handle_chat(payload, upstream::client()).await },
        "POST /api/chat stream=false — non-streaming chat. Acquires a VRAM lease for the duration.",
        "wylde_ollama::actions::chat",
    );

    // ── Streaming actions (2) ────────────────────────────────────────
    register_streaming_action_with_meta(
        "ollama.chat_stream",
        |payload: Value, sender| async move {
            chat::handle_chat_stream(payload, sender, upstream::client()).await;
        },
        "POST /api/chat stream=true — streaming chat. Lease acquired; client \
         disconnect triggers a fire-and-forget keep_alive=0 eject as a \
         conservative cancel mechanism (design doc Q2).",
        "wylde_ollama::actions::chat",
    );
    register_streaming_action_with_meta(
        "ollama.pull",
        |payload: Value, sender| async move {
            pull::handle_pull(payload, sender, upstream::client()).await;
        },
        "POST /api/pull stream=true — long-running model pull with NDJSON progress \
         and retry-on-transient-error (6 attempts, exponential backoff).",
        "wylde_ollama::actions::pull",
    );

    tracing::info!("wylde-ollama: registered {} actions", ALL_ACTIONS.len());
}

/// Signal stop. Currently a no-op — the service has no background
/// workers beyond the per-request handlers and the heartbeat task on
/// each live lease. Kept symmetric with the broker's API.
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
    use wylde_shared::ipc::{dispatch_action, list_actions};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_all_eleven_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let actions = list_actions();
        // list_actions returns unary actions only — the contract metadata
        // covers both. Assert the 9 unary entries (Phase 8 added
        // `ollama.preload`).
        for n in [
            "ollama.health",
            "ollama.list_models",
            "ollama.list_loaded",
            "ollama.show",
            "ollama.delete",
            "ollama.eject",
            "ollama.preload",
            "ollama.chat",
            "ollama.embed",
            "ollama.gc",
            "ollama.store_usage",
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
        // Don't blow up. Reset to keep the registry clean for siblings.
        reset_for_tests();
    }

    #[tokio::test]
    async fn dispatching_unknown_subaction_returns_no_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "ollama.bogus",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }
}
