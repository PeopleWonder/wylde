//! `models.*` pipe-action handlers. Rust port of
//! `Core/harness/pipe/_models.py` (the registry surface + Ollama-side
//! ops). Harness Slice 3a.
//!
//! ## Scope vs. the Python surface
//!
//! Python exposes eight `models.*` verbs. This module implements all
//! eight — they are backed by Rust subsystems already present in this
//! crate (the model registry, the routing profiles, the `model_state`
//! store) or by `wylde-ollama` over IPC:
//!
//! | verb               | backing                                   |
//! |--------------------|-------------------------------------------|
//! | `models.list`        | [`crate::model_registry::api::list_models`] |
//! | `models.get_profile` | [`crate::model_registry::routing::profiles::get_profile`] |
//! | `models.show`        | `ollama.show` (IPC)                        |
//! | `models.delete`      | `ollama.delete` (IPC) + capability forget |
//! | `models.unload`      | `ollama.eject` (IPC) + capability forget  |
//! | `models.set_active`  | [`crate::model_registry::model_state`]      |
//! | `models.set_default` | [`crate::model_registry::model_state`]      |
//! | `models.get_default` | [`crate::model_registry::model_state`]      |
//!
//! Two more verbs — `models.transcribe` / `models.synthesize` — used to
//! drive the Python Voice STT/TTS engines. They were retired at the
//! Phase-11.E voice cutover (STT/TTS moved in-process into `wylde-voice`,
//! reached via the `voice.*` actions) and deleted entirely in the
//! Bucket-A IPC cleanup. They were never registered on the Rust pipe.
//!
//! ## Flag gate — `WYLDE_HARNESS_MODELS_IMPL`
//!
//! **Slice 3b (2026-06-03) flipped the default to `rust`.** The handlers
//! are live unless the flag is an explicit `python` (the rollback path),
//! in which case every handler returns `not_implemented` — a
//! transport-class code the Python forwarder treats as "fall back to the
//! in-process Python driver." During Slice 3a the polarity was inverted
//! (default off) so a premature forward failed *loudly* rather than
//! silently — the failure mode the Slice 3 stop-finding flagged. Now that
//! the Python forwarder is wired and parity-tested (Slice 3b), Rust is
//! authoritative by default.

use async_trait::async_trait;
use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};

use crate::api::require_string;
use crate::model_registry::api::{list_models, live_ollama_probe};
use crate::model_registry::model_state;
use crate::model_registry::routing::profiles::get_profile;
use crate::model_registry::types::{Kind, ModelEntry};

/// Read `WYLDE_HARNESS_MODELS_IMPL`. **Slice 3b (2026-06-03) flipped the
/// default to `rust`**: the handlers are live unless the flag is an
/// explicit `python` (the rollback path); unset / any other value enables
/// them. Mirrors the Python `_models._models_impl()` forwarder's
/// clamp-to-default shape so the two halves of the strangler agree.
pub fn rust_enabled() -> bool {
    !matches!(
        std::env::var("WYLDE_HARNESS_MODELS_IMPL")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "python"
    )
}

/// The reply returned by every handler when the flag is an explicit
/// `python` (the rollback path). `not_implemented` is in the Python
/// forwarder's transport-fallback set, so the forward reverts to the
/// in-process Python body instead of erroring the caller.
fn disabled() -> Reply {
    Reply::err_msg(
        "not_implemented",
        "models.* Rust handlers are disabled by WYLDE_HARNESS_MODELS_IMPL=python \
         (rollback path — the in-process Python implementation handles the verb)",
    )
}

// ── Ollama IPC injection ───────────────────────────────────────────────

/// Indirection over the `ollama.*` IPC calls the model-management verbs
/// need, so unit tests can exercise the handler logic without a live
/// `wylde-ollama`. Mirrors the [`crate::model_registry::api::OllamaProbe`]
/// injection precedent used by `list_models`.
#[async_trait]
pub trait OllamaActions: Send + Sync {
    async fn call(&self, action: &str, payload: Value) -> Reply;
}

/// Production impl — dispatches against the configured `wylde-ollama`
/// service over the shared IPC transport.
pub struct LiveOllama {
    pub service: String,
}

#[async_trait]
impl OllamaActions for LiveOllama {
    async fn call(&self, action: &str, payload: Value) -> Reply {
        wylde_shared::ipc::send_action(&self.service, action, payload).await
    }
}

// ── models.list ────────────────────────────────────────────────────────

/// Shape the `list_models` result into the `{models, count, kind}`
/// envelope Python emits. Split out so the shaping is unit-testable
/// without scanning the HF cache or probing Ollama.
fn list_payload(entries: &[ModelEntry], kind_label: &str) -> Value {
    let models: Vec<Value> = entries.iter().map(ModelEntry::to_value).collect();
    json!({
        "models": models,
        "count": models.len(),
        "kind": kind_label,
    })
}

/// `models.list` — registry view, optionally filtered by `kind`.
/// Reply: `{models, count, kind}` where `kind` echoes the requested
/// filter or `"all"`.
pub async fn handle_list(payload: Value) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let raw_kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let (kind, kind_label) = match raw_kind {
        None => (None, "all".to_owned()),
        Some(k) => match Kind::parse(k) {
            Some(parsed) => (Some(parsed), parsed.as_str().to_owned()),
            // Unknown kind: Python's `list_models(kind=...)` would simply
            // match nothing rather than raise, so mirror that with an
            // empty list echoing the requested label.
            None => return Reply::ok(list_payload(&[], k)),
        },
    };

    // `list_models` is synchronous and the live Ollama probe blocks on an
    // IPC round-trip; keep it off the async worker.
    let entries =
        tokio::task::spawn_blocking(move || list_models(kind, &live_ollama_probe(), None))
            .await
            .unwrap_or_default();

    Reply::ok(list_payload(&entries, &kind_label))
}

// ── models.get_profile ─────────────────────────────────────────────────

/// `models.get_profile` — the routing profile for a model name.
/// Reply: `{name, profile}` where `profile` is `{}` when unknown.
pub async fn handle_get_profile(payload: Value) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let Some(name) = require_string(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    let profile = get_profile(&name).unwrap_or_else(|| json!({}));
    Reply::ok(json!({ "name": name, "profile": profile }))
}

// ── models.show / delete / unload (Ollama-side ops) ────────────────────

/// `models.show` — `/api/show` metadata for a locally-installed Ollama
/// model. Reply: the raw Ollama payload on success; `not_found` when the
/// model isn't installed; the upstream error otherwise (an Ollama outage
/// is surfaced honestly, not masked as `not_found`).
pub async fn handle_show<O: OllamaActions + ?Sized>(payload: Value, ollama: &O) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let Some(name) = require_string(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    let reply = ollama.call("ollama.show", json!({ "model": name })).await;
    if reply.ok {
        return Reply::ok(reply.data);
    }
    let code = reply
        .error
        .as_ref()
        .map(|e| e.code.as_str())
        .unwrap_or_default();
    if code == "model_not_found" {
        return Reply::err_msg("not_found", format!("model {name:?} not found"));
    }
    reply.error.map(Reply::err).unwrap_or_else(|| {
        Reply::err_msg("unavailable", format!("ollama.show failed for {name:?}"))
    })
}

/// `models.delete` — uninstall a model via `/api/delete` and drop its
/// cached capability flags. Reply: `{ok, name}` — `ok` mirrors Python's
/// boolean (false for a missing model OR a transport failure). The
/// non-ok branch is logged so a swallowed failure stays observable.
pub async fn handle_delete<O: OllamaActions + ?Sized>(payload: Value, ollama: &O) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let Some(name) = require_string(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    let reply = ollama.call("ollama.delete", json!({ "name": name })).await;
    finish_mutation("models.delete", &name, reply)
}

/// `models.unload` — evict a model from VRAM via `/api/generate`
/// `keep_alive=0` (the `ollama.eject` verb). Same `{ok, name}` shape and
/// capability-forget as delete.
pub async fn handle_unload<O: OllamaActions + ?Sized>(payload: Value, ollama: &O) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let Some(name) = require_string(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    let reply = ollama.call("ollama.eject", json!({ "model": name })).await;
    finish_mutation("models.unload", &name, reply)
}

/// Shared tail for delete/unload: on success drop the capability cache
/// entry; on failure log (so it isn't silent) and report `ok: false`.
fn finish_mutation(verb: &str, name: &str, reply: Reply) -> Reply {
    let ok = reply.ok;
    if ok {
        model_state::forget_model(name);
    } else {
        let err: Option<IpcError> = reply.error;
        tracing::warn!(
            verb,
            model = name,
            error = ?err,
            "ollama mutation returned non-ok; reporting ok=false (Python parity)"
        );
    }
    Reply::ok(json!({ "ok": ok, "name": name }))
}

// ── models.set_active / set_default / get_default (model_state) ────────

/// Validate an optional `model` field that may be a string, `null`, or
/// omitted. Returns `Ok(Some(s))` for a string, `Ok(None)` for null/
/// absent, `Err(msg)` for any other type.
fn optional_model_field<'a>(payload: &'a Value, type_msg: &str) -> Result<Option<&'a str>, String> {
    match payload.get("model") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(type_msg.to_owned()),
    }
}

/// `models.set_active` — persist the inference bar's current pick.
/// Reply: `{model}` — the persisted value, `""` when cleared.
pub async fn handle_set_active(payload: Value) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let model = match optional_model_field(&payload, "model must be a string or omitted") {
        Ok(m) => m,
        Err(msg) => return Reply::err_msg("bad_request", msg),
    };
    let new = model_state::set_active_model(model);
    Reply::ok(json!({ "model": new.unwrap_or_default() }))
}

/// `models.set_default` — persist the user's starred default. Reply:
/// `{ok, model}` where `model` is the persisted value (`null` when
/// cleared, so reads fall back to `WYLDE_DEFAULT_MODEL`).
pub async fn handle_set_default(payload: Value) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let model = match optional_model_field(&payload, "model must be a string or null") {
        Ok(m) => m,
        Err(msg) => return Reply::err_msg("bad_request", msg),
    };
    let new = model_state::set_default_model(model);
    Reply::ok(json!({ "ok": true, "model": new }))
}

/// `models.get_default` — the starred default (persisted → env →
/// `null`). Reply: `{model}`.
pub async fn handle_get_default(_payload: Value) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    Reply::ok(json!({ "model": model_state::get_default_model() }))
}

/// `models.get_effective` — the model whose defaults would apply to the
/// *next* chat turn, resolving the inference-bar pick first and the
/// starred default second (the maintainer's "B with A fallback").
///
/// Resolution order: active (`active_model.json`) → default
/// (`default_model.json`) → `WYLDE_DEFAULT_MODEL` env → `null`. The two
/// later arms are folded into [`model_state::get_default_model`], so this
/// is `active ?? default-with-env`.
///
/// Reply: `{model: <name>|null, source: "active"|"default"|"env"|null}`.
/// `source` distinguishes the inference-bar pick from the star/env so the
/// Settings header can explain *why* a model is showing.
pub async fn handle_get_effective(_payload: Value) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    if let Some(active) = model_state::get_active_model() {
        return Reply::ok(json!({ "model": active, "source": "active" }));
    }
    // No live pick → fall back to the star. `get_default_model` already
    // folds the env fallback, so distinguish persisted-vs-env by peeking
    // at the on-disk default before its env arm fires.
    match model_state::get_default_model() {
        Some(model) => {
            let source = if model_state::get_persisted_default().is_some() {
                "default"
            } else {
                "env"
            };
            Reply::ok(json!({ "model": model, "source": source }))
        }
        None => Reply::ok(json!({ "model": Value::Null, "source": Value::Null })),
    }
}

#[cfg(test)]
// These async tests hold the sync `TEST_ENV_LOCK` across the in-process
// handler `.await` to serialise env-var mutation against the sibling
// model_registry tests. The handlers never acquire `TEST_ENV_LOCK`, so
// there's no deadlock risk and the lint is a false positive here.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Set the impl flag on + point model_state at a fresh tempdir.
    /// Returns the dir guard.
    fn enabled_isolated() -> tempfile::TempDir {
        std::env::set_var("WYLDE_HARNESS_MODELS_IMPL", "rust");
        let td = tempdir().unwrap();
        std::env::set_var("ACTIVE_MODEL_PATH", td.path().join("active_model.json"));
        std::env::set_var("DEFAULT_MODEL_PATH", td.path().join("default_model.json"));
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        model_state::reset_for_tests();
        td
    }

    /// Fake Ollama that replays a queued reply per call and records the
    /// (action, payload) it saw.
    struct FakeOllama {
        reply: Mutex<Option<Reply>>,
        seen: Mutex<Vec<(String, Value)>>,
    }

    impl FakeOllama {
        fn new(reply: Reply) -> Self {
            Self {
                reply: Mutex::new(Some(reply)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl OllamaActions for FakeOllama {
        async fn call(&self, action: &str, payload: Value) -> Reply {
            self.seen
                .lock()
                .unwrap()
                .push((action.to_owned(), payload.clone()));
            self.reply
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Reply::ok(json!({})))
        }
    }

    #[tokio::test]
    async fn python_flag_disables_returns_not_implemented() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Slice 3b flipped the default to rust. Only an explicit `python`
        // disables the handlers (the rollback path) → not_implemented, which
        // the Python forwarder treats as "run the in-process body".
        std::env::set_var("WYLDE_HARNESS_MODELS_IMPL", "python");
        let r = handle_get_default(Value::Null).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_implemented");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn default_unset_is_enabled() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        // Default rust (Slice 3b): with the flag removed entirely the
        // handler still runs rather than returning not_implemented.
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
        let r = handle_get_default(Value::Null).await;
        assert!(r.ok);
        assert_eq!(r.data["model"], Value::Null);
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn set_then_get_default_round_trips() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();

        let r = handle_get_default(Value::Null).await;
        assert!(r.ok);
        assert_eq!(r.data["model"], Value::Null);

        let r = handle_set_default(json!({ "model": "qwen3:0.6b" })).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
        assert_eq!(r.data["model"], "qwen3:0.6b");

        let r = handle_get_default(Value::Null).await;
        assert_eq!(r.data["model"], "qwen3:0.6b");

        // Clearing → null again.
        let r = handle_set_default(json!({ "model": null })).await;
        assert_eq!(r.data["model"], Value::Null);
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn get_effective_resolution_chain() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();

        // Nothing set → null / null.
        let r = handle_get_effective(Value::Null).await;
        assert!(r.ok);
        assert_eq!(r.data["model"], Value::Null);
        assert_eq!(r.data["source"], Value::Null);

        // Only the env default → source "env".
        std::env::set_var("WYLDE_DEFAULT_MODEL", "env:model");
        model_state::reset_for_tests();
        let r = handle_get_effective(Value::Null).await;
        assert_eq!(r.data["model"], "env:model");
        assert_eq!(r.data["source"], "env");

        // A persisted star wins over env → source "default".
        let _ = handle_set_default(json!({ "model": "star:model" })).await;
        let r = handle_get_effective(Value::Null).await;
        assert_eq!(r.data["model"], "star:model");
        assert_eq!(r.data["source"], "default");

        // The active inference-bar pick wins over the star → source "active".
        let _ = handle_set_active(json!({ "model": "active:model" })).await;
        let r = handle_get_effective(Value::Null).await;
        assert_eq!(r.data["model"], "active:model");
        assert_eq!(r.data["source"], "active");

        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn set_active_clears_with_empty_and_rejects_non_string() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();

        let r = handle_set_active(json!({ "model": "m:1" })).await;
        assert_eq!(r.data["model"], "m:1");

        // Omitted/empty → cleared, reply carries "".
        let r = handle_set_active(json!({ "model": "" })).await;
        assert_eq!(r.data["model"], "");

        let r = handle_set_active(json!({ "model": 7 })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn get_profile_requires_name_and_defaults_empty() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        // Isolate the profile store too.
        std::env::set_var("MODEL_DATA_DIR", _td.path());

        let r = handle_get_profile(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");

        let r = handle_get_profile(json!({ "name": "ghost:model" })).await;
        assert!(r.ok);
        assert_eq!(r.data["name"], "ghost:model");
        assert_eq!(r.data["profile"], json!({}));
        std::env::remove_var("MODEL_DATA_DIR");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn list_unknown_kind_is_empty_not_error() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let r = handle_list(json!({ "kind": "bogus" })).await;
        assert!(r.ok);
        assert_eq!(r.data["count"], 0);
        assert_eq!(r.data["kind"], "bogus");
        assert_eq!(r.data["models"], json!([]));
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[test]
    fn list_payload_shapes_entries() {
        let entry = ModelEntry {
            id: "qwen3:0.6b".into(),
            kind: Kind::Llm,
            path: None,
            size_bytes: 0,
            loaded: true,
            provider: "ollama".into(),
            required_by: vec![],
            profile: None,
            last_accessed: None,
            chat_visible: true,
        };
        let v = list_payload(std::slice::from_ref(&entry), "llm");
        assert_eq!(v["count"], 1);
        assert_eq!(v["kind"], "llm");
        assert_eq!(v["models"][0]["id"], "qwen3:0.6b");
        assert_eq!(v["models"][0]["kind"], "llm");
    }

    #[tokio::test]
    async fn show_passes_through_ok_and_maps_not_found() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();

        let ok = FakeOllama::new(Reply::ok(json!({ "details": { "family": "qwen" } })));
        let r = handle_show(json!({ "name": "qwen3:0.6b" }), &ok).await;
        assert!(r.ok);
        assert_eq!(r.data["details"]["family"], "qwen");
        assert_eq!(ok.seen.lock().unwrap()[0].0, "ollama.show");

        let missing = FakeOllama::new(Reply::err(IpcError::new("model_not_found", "nope")));
        let r = handle_show(json!({ "name": "ghost" }), &missing).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn show_surfaces_transport_error_honestly() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let down = FakeOllama::new(Reply::err(IpcError::new("ollama_unreachable", "down")));
        let r = handle_show(json!({ "name": "qwen3:0.6b" }), &down).await;
        assert!(!r.ok);
        // NOT masked as not_found — the daemon outage is reported as-is.
        assert_eq!(r.error.unwrap().code, "ollama_unreachable");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn delete_ok_forgets_capability_and_reports_ok() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        model_state::mark_tool_failure("doomed:model");
        assert!(!model_state::model_supports_tools("doomed:model"));

        let del = FakeOllama::new(Reply::ok(json!({ "ok": true, "freed": true })));
        let r = handle_delete(json!({ "name": "doomed:model" }), &del).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
        assert_eq!(r.data["name"], "doomed:model");
        assert_eq!(del.seen.lock().unwrap()[0].0, "ollama.delete");
        // Capability flag dropped on successful delete.
        assert!(model_state::model_supports_tools("doomed:model"));
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn delete_failure_reports_ok_false_not_error() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let del = FakeOllama::new(Reply::err(IpcError::new("model_not_found", "gone")));
        let r = handle_delete(json!({ "name": "ghost" }), &del).await;
        // Parity with Python: the verb itself succeeds with ok=false.
        assert!(r.ok);
        assert_eq!(r.data["ok"], false);
        assert_eq!(r.data["name"], "ghost");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn unload_uses_eject_verb_with_model_field() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let eject = FakeOllama::new(Reply::ok(json!({ "ok": true })));
        let r = handle_unload(json!({ "name": "resident:model" }), &eject).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
        let seen = eject.seen.lock().unwrap();
        assert_eq!(seen[0].0, "ollama.eject");
        assert_eq!(seen[0].1["model"], "resident:model");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn mutation_verbs_require_name() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let fake = FakeOllama::new(Reply::ok(json!({})));
        for r in [
            handle_show(json!({}), &fake).await,
            handle_delete(json!({}), &fake).await,
            handle_unload(json!({}), &fake).await,
        ] {
            assert!(!r.ok);
            assert_eq!(r.error.unwrap().code, "bad_request");
        }
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }
}
