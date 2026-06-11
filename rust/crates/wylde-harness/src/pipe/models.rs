//! `models.*` pipe registrations (harness Slice 3a) -- registry surface
//! + Ollama-side ops. Split from `pipe.rs` per architecture-review R1.

use std::sync::Arc;

use serde_json::Value;
use wylde_shared::ipc::register_action_with_meta;

use crate::api::HarnessApi;

const HANDLER_MODULE_MODELS: &str = "wylde_harness::api::DefaultHarnessApi (models.*)";

/// Register the verbs in this family against `api`.
pub(super) fn install(api: &Arc<dyn HarnessApi>) {
    // ── models.* (harness Slice 3a) ──────────────────────────────────
    // Registered unconditionally; each handler self-gates on
    // WYLDE_HARNESS_MODELS_IMPL and returns `not_implemented` until the
    // flag is `rust`, so the Python path stays authoritative by default.

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.list",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_list(p).await }
        },
        "Registry view of every known model, optionally filtered by \
         `kind` (llm|stt|tts|vision|embed|wakeword). Merges the HF cache \
         scan, service manifests, the live Ollama tag probe, and routing \
         profiles. Payload {kind?}. Returns {models: [...], count, kind}.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.get_profile",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_get_profile(p).await }
        },
        "Routing profile (backend, backend_model, capabilities, \
         benchmark scores) for a model name. Payload {name}. Returns \
         {name, profile} where profile is {} when unknown.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.show",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_show(p).await }
        },
        "Fetch /api/show metadata for a locally-installed Ollama model \
         via wylde-ollama. Payload {name}. Returns the raw Ollama show \
         payload; `not_found` when the model isn't installed.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.delete",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_delete(p).await }
        },
        "Uninstall a model via Ollama /api/delete and drop its cached \
         capability flags. Payload {name}. Returns {ok, name} — ok is \
         false when the model was absent or Ollama was unreachable.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.unload",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_unload(p).await }
        },
        "Evict a model from VRAM (Ollama /api/generate keep_alive=0) and \
         drop its cached capability flags. Payload {name}. Returns \
         {ok, name}.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.set_active",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_set_active(p).await }
        },
        "Persist the inference bar's current model pick to \
         $DATA_DIR/active_model.json. Empty string / null clears it. \
         Payload {model?}. Returns {model} (the persisted value or \"\").",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.set_default",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_set_default(p).await }
        },
        "Persist the user's starred default model to \
         $DATA_DIR/default_model.json. null / empty clears it (reads then \
         fall back to WYLDE_DEFAULT_MODEL). Payload {model?}. Returns \
         {ok, model} where model is null when cleared.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.get_default",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_get_default(p).await }
        },
        "Return the starred default model: persisted choice, else the \
         WYLDE_DEFAULT_MODEL env, else null. No payload. Returns {model}.",
        HANDLER_MODULE_MODELS,
    );

    let a = Arc::clone(api);
    register_action_with_meta(
        "models.get_effective",
        move |p: Value| {
            let a = Arc::clone(&a);
            async move { a.models_get_effective(p).await }
        },
        "Resolve the model whose defaults apply to the next chat turn: \
         active inference-bar pick → starred default → WYLDE_DEFAULT_MODEL \
         env → null. No payload. Returns {model, source} where source is \
         one of active|default|env|null.",
        HANDLER_MODULE_MODELS,
    );
}
