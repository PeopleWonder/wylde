//! Warm model slots — keep the configured slot models resident so a Deep
//! turn never pays a cold load (implementation plan §6.3a, slice S2).
//!
//! **Warm slots, not hard pins** (the plan's recommendation, re-verified
//! against the code this slice): `wylde-ollama`'s VRAM leases are
//! service-internal, per-inference, RAII-dropped (`lease.rs`) — there is
//! no harness-holdable lease surface, and inventing one (§6.3b hard
//! pinning) adds a lease-lifecycle owner + a stale-pin failure mode for
//! no measured need. Instead: one `ollama.preload` per distinct slot
//! model (`POST /api/generate {model, prompt:"", keep_alive:"24h"}` —
//! load-without-generate). Ollama holds the models; the broker sees them
//! as synthetic leases via `/api/ps` and its keep-warm LRU protects
//! them. If something big evicts a slot anyway, the next call reloads —
//! a slow turn, never a failure (plan §9 R3).
//!
//! **Measured stake (2026-07-14, dev rig RTX 5080, the 12.9 GiB default
//! reasoner):** a cold PLAN call pays a multi-second model load on top
//! of think+generate; warm, `load_duration` is ~0.2 s. The breakdown
//! lives in `outputs/reasoning-s2-warm-slots-report.md`.
//!
//! ## Triggers
//!
//! * **Boot** — `service::install()` calls [`spawn_warm_slots`] (same
//!   no-runtime guard as the memory scheduler; sync test callers no-op).
//! * **Slot commit** — a successful `settings.reasoning.set` re-issues
//!   the loads. Preloading an already-resident model is a cheap no-op
//!   that refreshes its `keep_alive` window.
//!
//! **Identity:** `enabled == false` (the default) issues ZERO loads —
//! [`warm_models`] returns empty and the spawn declines. Nothing here
//! touches the turn path at all; residency only changes *when* a model
//! loads, never what a turn sends.

use serde_json::json;
use wylde_shared::ipc::{self, IpcError};

use super::config::ReasoningConfig;

/// Keep-alive for warmed slots — matches the `"24h"` every chat call
/// already sends, so a warmed model and a used model expire on the same
/// horizon.
pub const WARM_KEEP_ALIVE: &str = "24h";

/// The distinct model tags to keep warm for `cfg`, or empty when the
/// master toggle is off. Order: reasoner first (the deep turn's first
/// call — the biggest cold-load stake), then fast, then the effective
/// embedder ([`crate::memory::common::embed_model`] — env override wins,
/// S2's one-definition-of-the-embedder unification).
pub fn warm_models(cfg: &ReasoningConfig) -> Vec<String> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for tag in [
        cfg.slots.reasoner.as_str(),
        cfg.slots.fast.as_str(),
        &crate::memory::common::embed_model(),
    ] {
        if !tag.is_empty() && !out.iter().any(|t| t == tag) {
            out.push(tag.to_owned());
        }
    }
    out
}

/// Issue one preload per model through `call`, fail-soft: a failed load
/// is logged and the rest still load (an unreachable daemon degrades to
/// today's lazy-load behaviour, never an error). Returns the number of
/// loads that succeeded. Generic over the transport so the loop is
/// unit-testable without a pipe.
pub async fn warm_slots_via<F, Fut>(call: F, cfg: &ReasoningConfig) -> usize
where
    F: Fn(serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, IpcError>>,
{
    let mut ok = 0usize;
    for model in warm_models(cfg) {
        match call(json!({ "model": model, "keep_alive": WARM_KEEP_ALIVE })).await {
            Ok(_) => {
                tracing::info!("reasoning: warm slot resident: {model}");
                ok += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "reasoning: warm slot load failed for {model} ({}: {}) — will lazy-load",
                    e.code,
                    e.message
                );
            }
        }
    }
    ok
}

/// Production loader: preload every warm model through `ollama.preload`.
pub async fn warm_slots(cfg: &ReasoningConfig) -> usize {
    let service = crate::config::Config::get().ollama_service.clone();
    warm_slots_via(
        |payload| ipc::call_action(&service, "ollama.preload", payload),
        cfg,
    )
    .await
}

/// Fire-and-forget warm-up: reads the current config and spawns the
/// loader when reasoning is enabled and an async runtime exists (the
/// memory-scheduler guard — sync test callers fall through silently).
/// Returns whether a load task was actually spawned.
pub fn spawn_warm_slots(reason: &'static str) -> bool {
    let cfg = ReasoningConfig::current();
    if !cfg.enabled {
        return false;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!("reasoning: no async runtime at {reason}; warm slots not started");
        return false;
    }
    tokio::spawn(async move {
        let n = warm_slots(&cfg).await;
        tracing::info!("reasoning: warm-slot pass ({reason}) loaded {n} model(s)");
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn enabled_cfg() -> ReasoningConfig {
        ReasoningConfig {
            enabled: true,
            ..ReasoningConfig::default()
        }
    }

    #[test]
    fn disabled_config_warms_nothing() {
        // THE identity guard: default (off) config ⇒ zero loads.
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        assert!(warm_models(&ReasoningConfig::default()).is_empty());
    }

    #[test]
    fn single_mode_dedupes_the_shared_brain() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        // Default slots: fast == reasoner (Aaron's same-model decision) —
        // the shared brain loads once, the embedder separately.
        let models = warm_models(&enabled_cfg());
        assert_eq!(
            models,
            vec![
                super::super::config::DEFAULT_REASONER_MODEL.to_owned(),
                super::super::config::DEFAULT_EMBED_MODEL.to_owned(),
            ]
        );
    }

    #[test]
    fn split_mode_warms_three() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        let mut cfg = enabled_cfg();
        cfg.slots.fast = "qwen2.5:7b-instruct".into();
        let models = warm_models(&cfg);
        assert_eq!(models.len(), 3, "{models:?}");
        assert_eq!(models[0], cfg.slots.reasoner, "reasoner loads first");
        assert_eq!(models[1], "qwen2.5:7b-instruct");
    }

    #[test]
    fn embedder_env_override_wins() {
        // S2 unification: the env var stays the override over the slot.
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("WYLDE_EMBED_MODEL", "mxbai-embed-large");
        let models = warm_models(&enabled_cfg());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        assert!(
            models.iter().any(|m| m == "mxbai-embed-large"),
            "{models:?}"
        );
        assert!(
            !models
                .iter()
                .any(|m| m == super::super::config::DEFAULT_EMBED_MODEL),
            "slot value is overridden, not added: {models:?}"
        );
    }

    #[tokio::test]
    async fn loader_issues_one_preload_per_model_with_keep_alive() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        let calls = std::sync::Mutex::new(Vec::<serde_json::Value>::new());
        let n = warm_slots_via(
            |p| {
                calls.lock().unwrap().push(p);
                async { Ok(json!({"ok": true})) }
            },
            &enabled_cfg(),
        )
        .await;
        assert_eq!(n, 2, "default Single slots = shared brain + embedder");
        let calls = calls.lock().unwrap();
        for c in calls.iter() {
            assert_eq!(c["keep_alive"], json!(WARM_KEEP_ALIVE));
            assert!(c["model"].is_string());
        }
    }

    #[tokio::test]
    async fn loader_is_fail_soft_and_continues_past_errors() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        let attempts = AtomicUsize::new(0);
        let n = warm_slots_via(
            |_p| {
                let i = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if i == 0 {
                        Err(IpcError::new("ollama_unreachable", "connect refused"))
                    } else {
                        Ok(json!({"ok": true}))
                    }
                }
            },
            &enabled_cfg(),
        )
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "kept going after err");
        assert_eq!(n, 1, "only the successful load counts");
    }

    #[tokio::test]
    async fn loader_never_calls_when_disabled() {
        let calls = AtomicUsize::new(0);
        let n = warm_slots_via(
            |_p| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({"ok": true})) }
            },
            &ReasoningConfig::default(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "disabled ⇒ ZERO loads");
        assert_eq!(n, 0);
    }

    #[test]
    fn spawn_declines_without_runtime_or_when_disabled() {
        // No tokio runtime in a plain #[test] ⇒ even an enabled config
        // declines (the scheduler guard); default config declines first.
        assert!(!spawn_warm_slots("test"));
    }
}
