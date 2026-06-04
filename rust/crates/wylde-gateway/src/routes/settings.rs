//! `/api/settings` — Wylde service config read/write.
//!
//! Rust port of `Gateway/routes/settings.py`. Ollama runtime defaults
//! only: a JSON file at `$WYLDE_ROOT/data/settings/ollama.json` merged
//! onto a built-in default block; PUT writes only known keys (anything
//! not in the default schema is dropped server-side).
//!
//! The Python file shape note ("A future hardware-detection surface
//! can land back here as its own sub-router once a service owns the
//! responsibility") still applies — only `/ollama` is here today.
//!
//! ## Wire format
//!
//! Both verbs return `{ok: true, data: <merged-settings>}` — matches
//! Python's `proxy_core.ok(_read_ollama())`. The atomic write uses the
//! `<name>.tmp` → `rename` pattern, identical to Python's
//! `tmp.write_text(...) ; tmp.replace(OLLAMA_SETTINGS)`.
//!
//! ## Auth
//!
//! Every settings route gates on `require_local` (loopback + WyldeLink
//! CGNAT tier), matching the Python `settings.py`.

use std::path::PathBuf;

use axum::extract::Json;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Map, Value};

use crate::auth::require_local;
use crate::envelopes::success;

/// Locate the settings directory: `$WYLDE_ROOT/data/settings/`.
/// Matches Python's `Path(__file__).resolve().parents[2] / "data" /
/// "settings"`.
fn settings_dir() -> PathBuf {
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("data").join("settings")
}

fn ollama_settings_path() -> PathBuf {
    settings_dir().join("ollama.json")
}

/// Default Ollama runtime settings. Matches Python's `DEFAULT_OLLAMA`
/// dict byte-for-byte (key set + values).
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

/// Read the saved settings merged onto the defaults. If the file is
/// missing or unparseable, return a fresh copy of the defaults.
fn read_ollama() -> Map<String, Value> {
    let mut merged = default_ollama();
    let path = ollama_settings_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(Value::Object(saved)) = serde_json::from_str::<Value>(&text) {
            for (k, v) in saved {
                merged.insert(k, v);
            }
        }
    }
    merged
}

/// Merge `data` into the existing settings, filtering out any keys
/// that aren't in the default schema, and atomically replace the
/// on-disk file. Returns the merged map.
fn write_ollama(data: Map<String, Value>) -> Map<String, Value> {
    let dir = settings_dir();
    let _ = std::fs::create_dir_all(&dir);
    let defaults = default_ollama();
    let mut merged = read_ollama();
    for (k, v) in data {
        if defaults.contains_key(&k) {
            merged.insert(k, v);
        }
    }
    // Atomic write: write to <path>.tmp, then rename onto the target.
    let target = ollama_settings_path();
    let tmp = target.with_extension("tmp");
    if let Ok(serialized) = serde_json::to_string_pretty(&Value::Object(merged.clone())) {
        if std::fs::write(&tmp, serialized).is_ok() {
            let _ = std::fs::rename(&tmp, &target); // wylde-check: discard-result-ok
        }
    }
    merged
}

/// `GET /api/settings/ollama` — read current settings.
pub async fn get_ollama() -> Response {
    success(Value::Object(read_ollama()))
}

/// `PUT /api/settings/ollama` — merge incoming overrides, write back.
pub async fn put_ollama(body: Option<Json<Value>>) -> Response {
    let payload = match body {
        Some(Json(Value::Object(m))) => m,
        _ => Map::new(),
    };
    success(Value::Object(write_ollama(payload)))
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
mod tests {
    use super::*;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Serializes tests that mutate the process-global `WYLDE_ROOT`
    /// env var, since cargo runs tests in parallel by default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    }

    #[test]
    fn put_get_preserves_min_p() {
        // Regression: the schema previously whitelisted 8 keys and
        // dropped `min_p` server-side, so the gpui Settings "Min-p" row
        // rendered "—" and a PUT silently lost the value. Confirm a
        // round-trip now persists it.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ENV_LOCK serializes WYLDE_ROOT mutation across tests.
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let mut input = Map::new();
        input.insert("min_p".to_owned(), json!(0.05));
        let merged = write_ollama(input);
        assert_eq!(merged["min_p"], json!(0.05));

        // Confirm it survives a fresh read from disk.
        let read_back = read_ollama();
        assert_eq!(read_back["min_p"], json!(0.05));

        std::env::remove_var("WYLDE_ROOT");
    }

    #[test]
    fn write_drops_unknown_keys() {
        // Use a temp WYLDE_ROOT so the test doesn't touch the real
        // user data dir. Lock through the actual write_ollama call to
        // verify the whitelist filter.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ENV_LOCK serializes WYLDE_ROOT mutation across tests.
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let mut input = Map::new();
        input.insert("temperature".to_owned(), json!(0.42));
        input.insert("not_a_real_key".to_owned(), json!("ignored"));
        let merged = write_ollama(input);
        assert_eq!(merged["temperature"], json!(0.42));
        assert!(!merged.contains_key("not_a_real_key"));

        // Confirm it round-trips: read should return the same.
        let read_back = read_ollama();
        assert_eq!(read_back["temperature"], json!(0.42));

        std::env::remove_var("WYLDE_ROOT");
    }
}
