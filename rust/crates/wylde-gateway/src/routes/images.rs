//! `/api/images` — image generation + library proxy.
//!
//! Rust port of `Gateway/routes/images.py`. The image-gen service
//! (ComfyUI / friends) lives behind a shared HTTP base at
//! `http://127.0.0.1:8014`. Generated images come back base64-encoded
//! (typically < 5 MB) so the mobile client gets the bytes inline
//! without a second round trip.
//!
//! Library files live next to the image-gen service on disk at
//! `$WYLDE_ROOT/data/images/`. The library routes (`/library`,
//! `/library/:id`, `/library/:id` DELETE) read / serve / unlink them
//! directly — they don't touch the image-gen HTTP API.
//!
//! ## Wire format
//!
//! Success responses use the canonical `{ok: true, data: …}` wrapper
//! (matches Python's `proxy_core.ok(…)`). Failure responses use the
//! canonical nested envelope, same cross-wave convention picked by
//! wave 2c. Library entry shape preserved verbatim from Python:
//! `{id, filename, size_bytes, created_at, metadata}`; the per-image
//! GET adds `mime` and `data_b64` (base64-encoded raw bytes).
//!
//! ## Auth
//!
//! Every images route gates on `require_local` (loopback + WyldeLink
//! CGNAT tier), matching the Python `images.py`.

use std::path::{Path as StdPath, PathBuf};
use std::time::Duration;
use std::time::SystemTime;

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::auth::require_local;
use crate::envelopes::{failure, success, success_with_status};
use crate::proxy_core::{http_call, HttpMethod, HTTP_DEFAULT_TIMEOUT};

/// Image-gen service base URL. Matches Python's `IMAGE_GEN_URL`.
const IMAGE_GEN_URL: &str = "http://127.0.0.1:8014";

/// 600s — long enough for a ComfyUI generate roundtrip on a slow GPU.
/// Matches Python's `timeout=600.0`.
const GENERATE_TIMEOUT: Duration = Duration::from_secs(600);

/// Locate the image library directory: `$WYLDE_ROOT/data/images/`.
/// Matches Python's `Path(__file__).resolve().parents[2] / "data" /
/// "images"` (Gateway/routes/images.py → repo root → data/images).
fn library_dir() -> PathBuf {
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("data").join("images")
}

/// `POST /api/images/generate` — proxy to the image-gen service.
pub async fn generate(body: Option<Json<Value>>) -> Response {
    let payload = body.map(|Json(v)| v);
    forward_one_shot(
        &format!("{IMAGE_GEN_URL}/generate"),
        HttpMethod::Post,
        payload,
        GENERATE_TIMEOUT,
    )
    .await
}

/// `GET /api/images/library` — list locally-saved generated images.
pub async fn library() -> Response {
    let dir = library_dir();
    let mut items: Vec<Value> = Vec::new();
    if dir.exists() {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("png"))
                        .unwrap_or(false)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        // Python: `sorted(LIBRARY_DIR.glob("*.png"), reverse=True)` —
        // reverse lex order by path. Match exactly.
        entries.sort();
        entries.reverse();
        for path in entries {
            items.push(library_entry(&path));
        }
    }
    success(json!({ "images": items }))
}

/// `GET /api/images/library/:img_id` — return one image inline (base64).
pub async fn library_get(Path(img_id): Path<String>) -> Response {
    let dir = library_dir();
    let img = match find_image(&dir, &img_id) {
        Some(p) => p,
        None => {
            return failure("not_found", "image not in library", StatusCode::NOT_FOUND);
        }
    };
    let bytes = match std::fs::read(&img) {
        Ok(b) => b,
        Err(_) => {
            return failure("not_found", "image not in library", StatusCode::NOT_FOUND);
        }
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let suffix = img
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let mime = format!("image/{suffix}");
    let filename = img
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    let metadata = read_meta(&img);
    success(json!({
        "id": img_id,
        "filename": filename,
        "mime": mime,
        "data_b64": data_b64,
        "metadata": metadata,
    }))
}

/// `DELETE /api/images/library/:img_id` — drop every file matching
/// `<img_id>.*` from the library directory.
pub async fn library_delete(Path(img_id): Path<String>) -> Response {
    let dir = library_dir();
    let mut removed: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s == img_id)
                .unwrap_or(false)
                && std::fs::remove_file(&path).is_ok()
            {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    removed.push(name.to_owned());
                }
            }
        }
    }
    if removed.is_empty() {
        return failure("not_found", "image not in library", StatusCode::NOT_FOUND);
    }
    success(json!({ "deleted": removed }))
}

/// `GET /api/images/models` — proxy to the image-gen `/list_models`.
pub async fn list_models() -> Response {
    forward_one_shot(
        &format!("{IMAGE_GEN_URL}/list_models"),
        HttpMethod::Get,
        None,
        HTTP_DEFAULT_TIMEOUT,
    )
    .await
}

/// `GET /api/images/loras` — proxy to the image-gen `/list_loras`.
pub async fn list_loras() -> Response {
    forward_one_shot(
        &format!("{IMAGE_GEN_URL}/list_loras"),
        HttpMethod::Get,
        None,
        HTTP_DEFAULT_TIMEOUT,
    )
    .await
}

/// Build a Python-shape library entry. Skips the file if it can't be
/// stat'd (matches Python's `path.stat()` raising → caller would 500;
/// we just silently drop the entry).
fn library_entry(path: &StdPath) -> Value {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    let (size_bytes, created_at) = match std::fs::metadata(path) {
        Ok(m) => {
            let size = m.len();
            // Python uses mtime; match that (NOT ctime).
            let mtime_secs = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            (size, mtime_secs)
        }
        Err(_) => (0, 0.0),
    };
    let metadata = read_meta(path);
    json!({
        "id": id,
        "filename": filename,
        "size_bytes": size_bytes,
        "created_at": created_at,
        "metadata": metadata,
    })
}

/// Read the sidecar JSON at `<path>.json` (without extension swap —
/// Python's `path.with_suffix(".json")` REPLACES the extension, not
/// appends). Returns `{}` if missing or unparseable, matching the
/// Python `except Exception` branch.
fn read_meta(image: &StdPath) -> Value {
    let meta_path = image.with_extension("json");
    if !meta_path.exists() {
        return Value::Object(Map::new());
    }
    match std::fs::read_to_string(&meta_path) {
        Ok(s) => serde_json::from_str::<Value>(&s).unwrap_or_else(|_| Value::Object(Map::new())),
        Err(_) => Value::Object(Map::new()),
    }
}

/// Find the first .png/.jpg/.jpeg/.webp file whose stem equals `img_id`.
fn find_image(dir: &StdPath, img_id: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s == img_id)
            .unwrap_or(false)
        {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext_l = ext.to_ascii_lowercase();
                if matches!(ext_l.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                    return Some(path);
                }
            }
        }
    }
    None
}

async fn forward_one_shot(
    url: &str,
    method: HttpMethod,
    body: Option<Value>,
    timeout: Duration,
) -> Response {
    match http_call(url, method, body, timeout).await {
        Ok((status, value)) => success_with_status(value, status),
        Err((status, env)) => {
            let code = env
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_owned();
            let message = env
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            failure(&code, &message, status)
        }
    }
}

/// Build the `/api/images` sub-router.
pub fn router() -> Router {
    Router::new()
        .route(
            "/api/images/generate",
            post(generate).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/images/library",
            get(library).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/images/library/:img_id",
            get(library_get)
                .delete(library_delete)
                .route_layer(from_fn(require_local)),
        )
        .route(
            "/api/images/models",
            get(list_models).route_layer(from_fn(require_local)),
        )
        .route(
            "/api/images/loras",
            get(list_loras).route_layer(from_fn(require_local)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    /// Drive a route from a non-local caller; assert the canonical
    /// `403 auth_local_denied` envelope the `require_local` tier emits.
    async fn assert_local_denied(method: &str, uri: &str, body: Option<&str>) {
        let app = router();
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let mut request = req
            .body(match body {
                Some(b) => axum::body::Body::from(b.to_owned()),
                None => axum::body::Body::empty(),
            })
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} should 403 for a non-local caller"
        );
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn generate_rejects_non_local_caller() {
        assert_local_denied("POST", "/api/images/generate", Some(r#"{"prompt":"x"}"#)).await;
    }

    #[tokio::test]
    async fn library_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/images/library", None).await;
    }

    #[tokio::test]
    async fn library_get_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/images/library/abc", None).await;
    }

    #[tokio::test]
    async fn library_delete_rejects_non_local_caller() {
        assert_local_denied("DELETE", "/api/images/library/abc", None).await;
    }

    #[tokio::test]
    async fn list_models_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/images/models", None).await;
    }

    #[tokio::test]
    async fn list_loras_rejects_non_local_caller() {
        assert_local_denied("GET", "/api/images/loras", None).await;
    }

    #[test]
    fn read_meta_returns_empty_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        let img = tmp.path().join("nothere.png");
        let meta = read_meta(&img);
        assert!(meta.is_object());
        assert!(meta.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_meta_parses_sidecar_json() {
        let tmp = TempDir::new().unwrap();
        let img = tmp.path().join("foo.png");
        std::fs::write(tmp.path().join("foo.json"), r#"{"prompt":"sunset"}"#).unwrap();
        let meta = read_meta(&img);
        assert_eq!(meta["prompt"], "sunset");
    }

    #[test]
    fn read_meta_swallows_invalid_json() {
        let tmp = TempDir::new().unwrap();
        let img = tmp.path().join("foo.png");
        std::fs::write(tmp.path().join("foo.json"), "{not json}").unwrap();
        let meta = read_meta(&img);
        assert!(meta.is_object());
        assert!(meta.as_object().unwrap().is_empty());
    }

    #[test]
    fn find_image_picks_known_extensions() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bar.txt"), "skip me").unwrap();
        std::fs::write(tmp.path().join("bar.png"), [0u8; 4]).unwrap();
        let p = find_image(tmp.path(), "bar").expect("should find png");
        assert_eq!(p.extension().unwrap(), "png");
    }

    #[test]
    fn find_image_ignores_unknown_extensions() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bar.txt"), "skip me").unwrap();
        assert!(find_image(tmp.path(), "bar").is_none());
    }
}
