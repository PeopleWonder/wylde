//! `settings.ollama.*` + `settings.encryption.*` pipe registrations --
//! per-model inference overrides and the encryption-at-rest toggle
//! (OI-14). Split from `pipe.rs` per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_SETTINGS: &str = "wylde_harness::api::DefaultHarnessApi (settings.ollama.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── settings.ollama.* (per-model inference override store) ────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.ollama.get_overrides",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_get_overrides(p).await }
        },
        "Sparse per-model Ollama inference overrides. Payload {model, \
         profile?}. Returns {model, profile, overrides} where overrides \
         is the sparse map of only the keys the user set (empty when none).",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.ollama.set_overrides",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_set_overrides(p).await }
        },
        "Set/merge one per-model override key. Payload {model, key, value, \
         profile?}. Returns {model, profile, overrides} after the merge.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.ollama.clear_override",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_clear_override(p).await }
        },
        "Delete one per-model override key (the field falls back to its \
         placeholder). Payload {model, key, profile?}. Returns {model, \
         profile, overrides} with the remaining keys.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.ollama.list_models_with_overrides",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_ollama_list_models_with_overrides(p).await }
        },
        "List real model tags that have at least one stored override \
         (for the future profiles UI). Payload {profile?}. Returns \
         {profile, models}.",
        HANDLER_MODULE_SETTINGS,
    );

    // ── settings.encryption.* (encryption-at-rest toggle, OI-14) ──────

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.encryption.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_encryption_get(p).await }
        },
        "Whether encryption-at-rest is enabled (OI-14; default on). \
         Payload {}. Returns {enabled}.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.encryption.set",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_encryption_set(p).await }
        },
        "Persist the encryption-at-rest toggle. Payload {enabled}. Turning \
         it off rewrites each store as plaintext on its next save. Returns \
         {enabled}.",
        HANDLER_MODULE_SETTINGS,
    );

    // ── settings.concept_routing.* (routing master toggle, concept-routing
    //    plan §3) ───────────────────────────────────────────────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.concept_routing.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_concept_routing_get(p).await }
        },
        "The concept-routing config (master toggle + knobs). Payload {}. \
         Returns {enabled, curate_before_inject, mode, max_concepts, \
         abs_threshold, relative_floor, scope_to_active_region}. Default off.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.concept_routing.set",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_concept_routing_set(p).await }
        },
        "Persist the concept-routing config. Payload is a partial patch — any \
         omitted field keeps its current value (e.g. {enabled:true} flips just \
         the master toggle). Returns the persisted config.",
        HANDLER_MODULE_SETTINGS,
    );

    // ── settings.reasoning.* + reasoning.fit_check (agentic-reasoning S1:
    //    master toggle + model slots + advisory VRAM fit) ───────────────

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.reasoning.get",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_reasoning_get(p).await }
        },
        "The agentic-reasoning config (master toggle + model slots). Payload \
         {}. Returns {enabled, slots{embedder,fast,reasoner}, mode, \
         default_depth, auto_escalate, replan_budget, \
         tier_budgets{think_harder,ultrathink}, reflect_gate}. Default off; \
         default slots run PLAN and EXECUTE on the same model (mode single).",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "settings.reasoning.set",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.settings_reasoning_set(p).await }
        },
        "Persist the agentic-reasoning config. Payload is a partial patch — \
         any omitted field keeps its current value (e.g. {enabled:true} flips \
         just the master toggle). Returns the persisted config.",
        HANDLER_MODULE_SETTINGS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "reasoning.fit_check",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.reasoning_fit_check(p).await }
        },
        "Price the (given or configured) model slots against the live VRAM \
         budget. Payload {slots?, mode?}. Returns {co_resident, \
         total_estimate_bytes, budget_bytes, suggested_mode, warnings, \
         suggestion?}. Advisory only — warns, never blocks; unreachable \
         Ollama/broker degrade to warnings.",
        HANDLER_MODULE_SETTINGS,
    );
}
