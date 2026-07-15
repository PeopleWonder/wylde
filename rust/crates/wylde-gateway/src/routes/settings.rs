//! `/api/settings` — Wylde service config read/write.
//!
//! Ollama runtime defaults only today. The route is a **thin facade**
//! over the harness's per-model override store: it does NOT read the old
//! flat `data/settings/ollama.json` any more. Both the GET and PUT verbs
//! call the same in-process harness handlers the named-pipe surface uses
//! (`models.get_effective` + `settings.ollama.{get,set}_overrides`) and
//! compose over the same [`default_ollama`] fallback table, so the TCP and
//! pipe surfaces return byte-identical blocks for the same backing state.
//!
//! ## Why a facade
//!
//! The gpui GUI talks to services over named pipes. The Gateway settings
//! route was registered only on its axum/TCP surface, so a pipe read of it
//! always failed (`unknown_action`) and the Settings panel rendered all
//! dashes. The fix moved the canonical surface onto harness verbs; this
//! route now layers over those *same* verbs rather than its own flat-file
//! store, closing the drift trap where TCP and pipe could disagree.
//!
//! The Python file shape note ("A future hardware-detection surface can
//! land back here as its own sub-router once a service owns the
//! responsibility") still applies — only `/ollama` is here today.
//!
//! ## Wire format
//!
//! Both verbs return `{ok: true, data: <nine-key-block>}`. `data` is the
//! [`default_ollama`] table with the *effective* model's stored per-model
//! overrides merged on top. With no model selected (or the harness
//! `models.*` handlers disabled by the rollback flag) it is the bare
//! defaults table.
//!
//! ## Auth
//!
//! Every settings route gates on `require_local` (loopback + WyldeLink
//! CGNAT tier), matching the Python `settings.py`.

use axum::extract::Json;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Map, Value};

use wylde_harness::model_registry::actions::handle_get_effective;
use wylde_harness::settings::actions::{handle_get_overrides, handle_set_overrides};

use crate::auth::require_local;
use crate::envelopes::success;

/// Default Ollama runtime settings — the global fallback table. Matches
/// Python's `DEFAULT_OLLAMA` dict (key set + values), plus `min_p` (added
/// in the Bucket-A schema fix). These nine keys are the schema: a PUT key
/// outside this set is dropped, and a GET always returns exactly these
/// nine, with any stored override layered on top.
fn default_ollama() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("temperature".to_owned(), json!(0.7));
    m.insert("top_k".to_owned(), json!(40));
    m.insert("top_p".to_owned(), json!(0.9));
    m.insert("num_ctx".to_owned(), json!(8192));
    m.insert("keep_alive".to_owned(), json!("5m"));
    m.insert("repeat_penalty".to_owned(), json!(1.1));
    m.insert("num_predict".to_owned(), json!(-1));
    m.insert("min_p".to_owned(), json!(0.0));
    m.insert("seed".to_owned(), json!(0));
    m
}

/// Resolve the model whose defaults apply to the next chat turn, via the
/// in-process `models.get_effective` verb (active inference-bar pick →
/// starred default → `WYLDE_DEFAULT_MODEL` → none). `None` when nothing is
/// selected, or when the `models.*` Rust handlers are disabled by the
/// `WYLDE_HARNESS_MODELS_IMPL=python` rollback flag (the reply is then
/// `not ok`, which we treat as "no model" → bare defaults).
async fn effective_model() -> Option<String> {
    let reply = handle_get_effective(Value::Null).await;
    if !reply.ok {
        return None;
    }
    reply
        .data
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The model's sparse stored overrides via the in-process
/// `settings.ollama.get_overrides` verb. `{}` when none are stored.
async fn overrides_for(model: &str) -> Map<String, Value> {
    let reply = handle_get_overrides(json!({ "model": model })).await;
    reply
        .data
        .get("overrides")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Compose the nine-key effective Ollama block exactly as the pipe surface
/// would: [`default_ollama`] with the effective model's stored per-model
/// overrides merged on top. Out-of-schema override keys (shouldn't exist,
/// but the store is sparse) are ignored so the result is always exactly
/// the nine schema keys.
async fn effective_ollama() -> Map<String, Value> {
    let mut merged = default_ollama();
    if let Some(model) = effective_model().await {
        for (k, v) in overrides_for(&model).await {
            if merged.contains_key(&k) {
                merged.insert(k, v);
            }
        }
    }
    merged
}

/// `GET /api/settings/ollama` — the effective Ollama block for the active
/// model. Thin facade over the harness verbs (NOT the retired flat
/// `ollama.json`), so it can't drift from the pipe surface.
pub async fn get_ollama() -> Response {
    success(Value::Object(effective_ollama().await))
}

/// `PUT /api/settings/ollama` — merge incoming overrides into the active
/// model's per-model store via `settings.ollama.set_overrides`, then return
/// the recomposed effective block. Keys outside the [`default_ollama`]
/// schema are dropped (parity with the old whitelist). With no active
/// model there's nowhere to store a per-model override, so the write is a
/// logged no-op and the response is the bare defaults block.
pub async fn put_ollama(body: Option<Json<Value>>) -> Response {
    let payload = match body {
        Some(Json(Value::Object(m))) => m,
        _ => Map::new(),
    };
    match effective_model().await {
        Some(model) => {
            let schema = default_ollama();
            for (key, value) in payload {
                if !schema.contains_key(&key) {
                    continue; // out-of-schema key — drop it, like the old whitelist
                }
                let reply = handle_set_overrides(json!({
                    "model": &model,
                    "key": key,
                    "value": value,
                }))
                .await;
                if !reply.ok {
                    tracing::warn!(
                        model = %model,
                        error = ?reply.error,
                        "settings.ollama.set_overrides returned non-ok"
                    );
                }
            }
        }
        None => {
            tracing::warn!(
                "PUT /api/settings/ollama with no active model; per-model \
                 overrides need a selected model, so the write is a no-op"
            );
        }
    }
    success(Value::Object(effective_ollama().await))
}

/// Build the `/api/settings` sub-router.
pub fn router() -> Router {
    Router::new().route(
        "/api/settings/ollama",
        get(get_ollama)
            .put(put_ollama)
            .route_layer(from_fn(require_local)),
    )
}

#[cfg(test)]
// The state-backed tests hold the sync `ENV_LOCK` across handler `.await`s
// to serialise the process-global model_state + override-store env vars;
// the handlers never take the lock, so there's no deadlock and the lint is
// a false positive here.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tower::ServiceExt;

    use wylde_harness::model_registry::actions::handle_set_active;
    use wylde_harness::model_registry::model_state;

    /// Serialises tests that mutate the process-global model_state caches
    /// and the `DATA_DIR` / `WYLDE_DATA_DIR` env vars, since cargo runs
    /// tests in parallel by default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Point both the model_state selection files and the override store at
    /// `dir`, enable the Rust `models.*` handlers, and drop any cached
    /// selection. `model_state` reads `DATA_DIR/active_model.json`;
    /// `ollama_overrides` resolves its store root from `WYLDE_DATA_DIR`.
    fn isolate(dir: &std::path::Path) {
        std::env::set_var("DATA_DIR", dir);
        std::env::set_var("WYLDE_DATA_DIR", dir);
        std::env::set_var("WYLDE_HARNESS_MODELS_IMPL", "rust");
        std::env::remove_var("ACTIVE_MODEL_PATH");
        std::env::remove_var("DEFAULT_MODEL_PATH");
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        model_state::reset_for_tests();
    }

    fn unisolate() {
        std::env::remove_var("DATA_DIR");
        std::env::remove_var("WYLDE_DATA_DIR");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
        model_state::reset_for_tests();
    }

    /// Drive a route via a loopback caller (passes `require_local`) and
    /// return the status + parsed `{ok, data}` envelope.
    async fn call_local(method: &str, body: Option<&str>) -> (StatusCode, Value) {
        let app = router();
        let builder = Request::builder()
            .method(method)
            .uri("/api/settings/ollama");
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(axum::body::Body::from(b.to_owned()))
                .unwrap(),
            None => builder.body(axum::body::Body::empty()).unwrap(),
        };
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5005))));
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        (status, v)
    }

    #[tokio::test]
    async fn get_rejects_non_local_caller() {
        let app = router();
        let mut req = Request::builder()
            .uri("/api/settings/ollama")
            .body(axum::body::Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn put_rejects_non_local_caller() {
        let app = router();
        let mut req = Request::builder()
            .method("PUT")
            .uri("/api/settings/ollama")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"temperature":0.5}"#))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn default_ollama_has_all_python_keys() {
        let d = default_ollama();
        for key in [
            "temperature",
            "top_k",
            "top_p",
            "num_ctx",
            "keep_alive",
            "repeat_penalty",
            "num_predict",
            "min_p",
            "seed",
        ] {
            assert!(d.contains_key(key), "missing default key: {key}");
        }
        assert_eq!(d.len(), 9, "schema is exactly nine keys");
    }

    /// The whole point of the facade: the TCP route returns the *same*
    /// nine-key block the named-pipe surface assembles from the same two
    /// verbs + the same defaults table. Seed an active model with one
    /// stored override, then compare surface-for-surface.
    #[tokio::test]
    async fn tcp_facade_matches_pipe_verb_composition() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        isolate(tmp.path());

        // Seed: an active model with one stored per-model override.
        assert!(
            handle_set_active(json!({ "model": "parity:model" }))
                .await
                .ok
        );
        assert!(
            handle_set_overrides(json!({
                "model": "parity:model",
                "key": "temperature",
                "value": 0.42,
            }))
            .await
            .ok
        );

        // Surface 1 — the TCP facade.
        let (status, envelope) = call_local("GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(envelope["ok"], json!(true));
        let tcp_data = envelope["data"].as_object().unwrap().clone();

        // Surface 2 — assemble the block straight from the two verbs + the
        // shared defaults table, the way the pipe surface does.
        let mut pipe_data = default_ollama();
        let eff = handle_get_effective(Value::Null).await;
        let model = eff.data["model"].as_str().unwrap().to_owned();
        let got = handle_get_overrides(json!({ "model": model })).await;
        for (k, v) in got.data["overrides"].as_object().unwrap() {
            if pipe_data.contains_key(k) {
                pipe_data.insert(k.clone(), v.clone());
            }
        }

        // Byte-identical, key-for-key.
        assert_eq!(Value::Object(tcp_data.clone()), Value::Object(pipe_data));
        // And the override actually took — proves we read the per-model
        // store, not the dead flat ollama.json.
        assert_eq!(tcp_data["temperature"], json!(0.42));
        assert_eq!(tcp_data.len(), 9);

        unisolate();
    }

    /// PUT proxies through `settings.ollama.set_overrides` for the active
    /// model: known keys persist (visible via both the GET facade and the
    /// raw verb), unknown keys are dropped.
    #[tokio::test]
    async fn put_persists_known_override_and_drops_unknown() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        isolate(tmp.path());
        assert!(handle_set_active(json!({ "model": "put:model" })).await.ok);

        // PUT a known key + an out-of-schema key.
        let (status, envelope) =
            call_local("PUT", Some(r#"{"temperature":0.33,"not_a_real_key":"x"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        let data = envelope["data"].as_object().unwrap();
        assert_eq!(data["temperature"], json!(0.33));
        assert!(!data.contains_key("not_a_real_key"));

        // GET reflects it.
        let (_, after) = call_local("GET", None).await;
        assert_eq!(after["data"]["temperature"], json!(0.33));

        // And the raw verb confirms the per-model store was written — and
        // the unknown key never reached it.
        let stored = handle_get_overrides(json!({ "model": "put:model" })).await;
        assert_eq!(stored.data["overrides"]["temperature"], json!(0.33));
        assert!(stored.data["overrides"]
            .as_object()
            .unwrap()
            .get("not_a_real_key")
            .is_none());

        unisolate();
    }

    /// With no model selected, GET falls back to the bare nine-key defaults
    /// table rather than erroring.
    #[tokio::test]
    async fn get_with_no_model_returns_defaults() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        isolate(tmp.path());

        let (status, envelope) = call_local("GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            envelope["data"],
            Value::Object(default_ollama()),
            "no model → bare defaults"
        );

        unisolate();
    }
}
