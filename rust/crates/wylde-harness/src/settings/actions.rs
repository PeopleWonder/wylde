//! `settings.ollama.*` pipe-action handlers — the per-model Ollama
//! inference override store's wire surface.
//!
//! These are brand-new verbs with no Python counterpart, so unlike the
//! `models.*` family there is no strangler flag gate: they are always
//! live. They back the Settings → "Ollama inference" panel's write path
//! (typing a value → [`handle_set_overrides`]; the ↺ reset →
//! [`handle_clear_override`]) and its sparse read ([`handle_get_overrides`]).
//!
//! Payloads accept an optional `profile` field (default
//! [`ollama_overrides::DEFAULT_PROFILE`]) so the future model-profiles UI
//! can target a non-default profile without a verb change.

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::settings::ollama_overrides;

/// Pull the `profile` field or fall back to the default profile.
fn profile_of(payload: &Value) -> String {
    payload
        .get("profile")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(ollama_overrides::DEFAULT_PROFILE)
        .to_owned()
}

/// Require a non-empty string `model` field, or `None` (the caller maps
/// that to a `bad_request` reply — kept a thin `Option` rather than a
/// `Result<_, Reply>` so the large `Reply` doesn't bloat the error path).
fn require_model(payload: &Value) -> Option<String> {
    payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

/// `settings.encryption.get {}` — whether encryption-at-rest (OI-14) is on.
/// Reply: `{enabled}`. Backs the Settings "Encrypt local data at rest" toggle;
/// served from the harness so the canonical `data_dir` (the one the stores
/// use) is the single source of truth across processes.
pub async fn handle_encryption_get(_payload: Value) -> Reply {
    Reply::ok(json!({ "enabled": wylde_shared::encryption::is_encryption_enabled() }))
}

/// `settings.encryption.set {enabled}` — persist the encryption-at-rest
/// toggle. Default on; turning it **off** makes each store rewrite as
/// plaintext on its next save. Reply: `{enabled}` (the persisted value).
pub async fn handle_encryption_set(payload: Value) -> Reply {
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return Reply::err_msg("bad_request", "enabled (bool) is required");
    };
    match wylde_shared::encryption::set_encryption_enabled(enabled) {
        Ok(()) => Reply::ok(json!({ "enabled": enabled })),
        Err(e) => Reply::err_msg("io_error", format!("persist encryption pref: {e}")),
    }
}

/// `settings.ollama.get_overrides {model, profile?}` — the sparse
/// overrides stored for `model`, `{}` when none. Reply:
/// `{model, profile, overrides}`.
pub async fn handle_get_overrides(payload: Value) -> Reply {
    let Some(model) = require_model(&payload) else {
        return Reply::err_msg("bad_request", "model is required");
    };
    let profile = profile_of(&payload);
    let overrides = ollama_overrides::get_overrides(&profile, &model);
    Reply::ok(json!({
        "model": model,
        "profile": profile,
        "overrides": Value::Object(overrides),
    }))
}

/// `settings.ollama.set_overrides {model, key, value, profile?}` —
/// set/merge a single key. Reply: `{model, profile, overrides}` (the full
/// sparse map after the merge).
pub async fn handle_set_overrides(payload: Value) -> Reply {
    let Some(model) = require_model(&payload) else {
        return Reply::err_msg("bad_request", "model is required");
    };
    let profile = profile_of(&payload);
    let Some(key) = payload.get("key").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Reply::err_msg("bad_request", "key is required");
    };
    // `value` may be any JSON scalar (number / string). Absent value is a
    // bad request — clearing is `clear_override`, not a null set.
    let Some(value) = payload.get("value") else {
        return Reply::err_msg("bad_request", "value is required");
    };
    let overrides = ollama_overrides::set_override(&profile, &model, key, value.clone());
    Reply::ok(json!({
        "model": model,
        "profile": profile,
        "overrides": Value::Object(overrides),
    }))
}

/// `settings.ollama.clear_override {model, key, profile?}` — delete one
/// override key (the field falls back to its placeholder). Reply:
/// `{model, profile, overrides}` (remaining sparse map).
pub async fn handle_clear_override(payload: Value) -> Reply {
    let Some(model) = require_model(&payload) else {
        return Reply::err_msg("bad_request", "model is required");
    };
    let profile = profile_of(&payload);
    let Some(key) = payload.get("key").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Reply::err_msg("bad_request", "key is required");
    };
    let overrides = ollama_overrides::clear_override(&profile, &model, key);
    Reply::ok(json!({
        "model": model,
        "profile": profile,
        "overrides": Value::Object(overrides),
    }))
}

/// `settings.ollama.list_models_with_overrides {profile?}` — real model
/// tags with at least one stored override (for the future profiles UI).
/// Reply: `{profile, models}`.
pub async fn handle_list_models_with_overrides(payload: Value) -> Reply {
    let profile = profile_of(&payload);
    let models = ollama_overrides::list_models_with_overrides(&profile);
    Reply::ok(json!({ "profile": profile, "models": models }))
}

#[cfg(test)]
// The round-trip test holds the sync `TEST_ENV_LOCK` across handler
// `.await`s to serialise `WYLDE_DATA_DIR` mutation; the handlers never
// take the lock, so there's no deadlock and the lint is a false positive.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;

    #[tokio::test]
    async fn set_then_get_round_trips_via_verbs() {
        // Hold the env lock across the awaits to serialise WYLDE_DATA_DIR
        // mutation against sibling store tests; the handlers never take
        // the lock so there's no deadlock.
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());

        let set = handle_set_overrides(json!({
            "model": "llama3.2:3b",
            "key": "temperature",
            "value": 0.7,
        }))
        .await;
        assert!(set.ok);
        assert_eq!(set.data["overrides"]["temperature"], json!(0.7));

        let got = handle_get_overrides(json!({ "model": "llama3.2:3b" })).await;
        assert!(got.ok);
        assert_eq!(got.data["overrides"]["temperature"], json!(0.7));
        assert_eq!(got.data["profile"], json!("default"));

        let cleared = handle_clear_override(json!({
            "model": "llama3.2:3b",
            "key": "temperature",
        }))
        .await;
        assert!(cleared.ok);
        assert!(cleared.data["overrides"].as_object().unwrap().is_empty());

        std::env::remove_var("WYLDE_DATA_DIR");
    }

    #[tokio::test]
    async fn get_overrides_requires_model() {
        let r = handle_get_overrides(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().map(|e| e.code.as_str()), Some("bad_request"));
    }

    #[tokio::test]
    async fn set_requires_key_and_value() {
        let no_key = handle_set_overrides(json!({ "model": "m:1", "value": 1 })).await;
        assert_eq!(
            no_key.error.as_ref().map(|e| e.code.as_str()),
            Some("bad_request")
        );
        let no_value = handle_set_overrides(json!({ "model": "m:1", "key": "seed" })).await;
        assert_eq!(
            no_value.error.as_ref().map(|e| e.code.as_str()),
            Some("bad_request")
        );
    }

    #[tokio::test]
    async fn encryption_get_set_round_trip() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());
        // The env override would mask the pref file these verbs write; clear it.
        std::env::remove_var("WYLDE_ENCRYPTION_AT_REST");

        // Default on.
        assert_eq!(handle_encryption_get(json!({})).await.data["enabled"], true);
        // Toggle off persists.
        let set = handle_encryption_set(json!({ "enabled": false })).await;
        assert_eq!(set.data["enabled"], false);
        assert_eq!(handle_encryption_get(json!({})).await.data["enabled"], false);
        // Back on.
        handle_encryption_set(json!({ "enabled": true })).await;
        assert_eq!(handle_encryption_get(json!({})).await.data["enabled"], true);
        // Missing field → bad_request.
        let bad = handle_encryption_set(json!({})).await;
        assert_eq!(bad.error.as_ref().map(|e| e.code.as_str()), Some("bad_request"));

        std::env::remove_var("WYLDE_DATA_DIR");
    }
}
