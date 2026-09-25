//! Router-level `/v1` tests (MCP-router style): auth, methods, the rate
//! limit, `/v1/models` and `/v1/embeddings` over a fake backend.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wylde_shared::ipc::IpcError;

use super::backend::testing::FakeBackend;
use super::*;
use crate::auth::token_cache::{global as token_cache, Device};

/// A fresh verified token (unique per call so tests never share buckets).
async fn token() -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let t = format!("openai-test-token-{n}-{}", std::process::id());
    token_cache()
        .insert(
            t.clone(),
            Device {
                device_id: format!("openai-dev-{n}"),
                tier: "read_only".into(),
            },
        )
        .await;
    t
}

fn registry() -> Value {
    json!({"models": [
        {"id": "hf.co/u/Coder-GGUF:IQ3", "kind": "llm", "chat_visible": true},
        {"id": "hidden:1", "kind": "llm", "chat_visible": false},
        {"id": "nomic-embed-text:latest", "kind": "embed", "chat_visible": false},
        {"id": "whisper", "kind": "stt", "chat_visible": false},
    ], "count": 4, "kind": "all"})
}

fn app(backend: FakeBackend, limit: u32) -> Router {
    router_with(
        Arc::new(backend),
        Aliases::parse(
            "coder=hf.co/u/Coder-GGUF:IQ3,embed=nomic-embed-text:latest,ghost=missing:1",
        ),
        RateLimiter::new(limit),
    )
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(
            b.header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v, headers)
}

fn assert_openai_error(v: &Value, code: &str) {
    assert_eq!(v["error"]["code"], code, "body: {v}");
    assert!(v["error"]["type"].is_string(), "body: {v}");
    assert!(
        v.get("ok").is_none(),
        "must not use the Wylde envelope: {v}"
    );
}

// ── auth, methods, rate limit ──────────────────────────────────────────

#[tokio::test]
async fn missing_token_is_openai_401() {
    let app = app(FakeBackend::new(Ok(registry())), 100);
    let (status, v, _) = send(&app, "GET", "/v1/models", None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_openai_error(&v, "invalid_api_key");
}

#[tokio::test]
async fn malformed_authorization_is_openai_401() {
    let app = app(FakeBackend::new(Ok(registry())), 100);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v: Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), 4096).await.unwrap()).unwrap();
    assert_openai_error(&v, "invalid_api_key");
}

#[tokio::test]
async fn get_on_embeddings_is_405() {
    let app = app(FakeBackend::new(Ok(registry())), 100);
    let t = token().await;
    let (status, _, _) = send(&app, "GET", "/v1/embeddings", Some(&t), "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn over_the_v1_limit_is_openai_429_with_retry_after() {
    let app = app(FakeBackend::new(Ok(registry())), 2);
    let t = token().await;
    for _ in 0..2 {
        let (status, _, _) = send(&app, "GET", "/v1/models", Some(&t), "").await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, v, headers) = send(&app, "GET", "/v1/models", Some(&t), "").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_openai_error(&v, "rate_limit_exceeded");
    let retry: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry));
}

// ── /v1/models ─────────────────────────────────────────────────────────

#[tokio::test]
async fn models_lists_chat_and_embed_models_plus_live_aliases() {
    let app = app(FakeBackend::new(Ok(registry())), 100);
    let t = token().await;
    let (status, v, _) = send(&app, "GET", "/v1/models", Some(&t), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["object"], "list");
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            "hf.co/u/Coder-GGUF:IQ3",
            "nomic-embed-text:latest",
            "coder",
            "embed"
        ],
        "hidden llm, stt and the dangling 'ghost' alias are excluded"
    );
    let coder = &v["data"][2];
    assert_eq!(coder["object"], "model");
    assert_eq!(coder["owned_by"], "wylde");
    assert_eq!(coder["root"], "hf.co/u/Coder-GGUF:IQ3");
}

#[tokio::test]
async fn models_falls_back_to_ollama_when_harness_is_down() {
    let mut fake = FakeBackend::new(Err(IpcError::new("pipe_unavailable", "harness down")));
    fake.ollama =
        Ok(json!({"models": [{"name": "nomic-embed-text:latest"}, {"name": "llama3.2:3b"}]}));
    let app = app(fake, 100);
    let t = token().await;
    let (status, v, _) = send(&app, "GET", "/v1/models", Some(&t), "").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["nomic-embed-text:latest", "llama3.2:3b", "embed"]);
}

#[tokio::test]
async fn models_get_resolves_slashed_ids_and_aliases_and_404s_unknown() {
    let app = app(FakeBackend::new(Ok(registry())), 100);
    let t = token().await;
    let (status, v, _) = send(
        &app,
        "GET",
        "/v1/models/hf.co/u/Coder-GGUF:IQ3",
        Some(&t),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "hf.co/u/Coder-GGUF:IQ3");
    let (status, v, _) = send(&app, "GET", "/v1/models/coder", Some(&t), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["root"], "hf.co/u/Coder-GGUF:IQ3");
    let (status, v, _) = send(&app, "GET", "/v1/models/ghost", Some(&t), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_openai_error(&v, "model_not_found");
}

// ── /v1/embeddings ─────────────────────────────────────────────────────

#[tokio::test]
async fn embeddings_happy_path_resolves_alias_and_reshapes() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.embed = Ok(json!({
        "model": "nomic-embed-text:latest",
        "embeddings": [[0.1, 0.2], [0.3, 0.4]],
        "prompt_eval_count": 7
    }));
    let fake = Arc::new(fake);
    let app = router_with(
        fake.clone(),
        Aliases::parse("embed=nomic-embed-text:latest"),
        RateLimiter::new(100),
    );
    let t = token().await;
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/embeddings",
        Some(&t),
        r#"{"model":"embed","input":["a","b"],"encoding_format":"float"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["object"], "list");
    assert_eq!(v["model"], "embed");
    assert_eq!(v["data"][0]["object"], "embedding");
    assert_eq!(v["data"][1]["index"], 1);
    assert_eq!(v["data"][1]["embedding"], json!([0.3, 0.4]));
    assert_eq!(v["usage"]["prompt_tokens"], 7);
    let calls = fake.embed_calls.lock().unwrap();
    assert_eq!(
        calls[0],
        ("nomic-embed-text:latest".to_owned(), json!(["a", "b"]))
    );
}

#[tokio::test]
async fn embeddings_unknown_model_is_openai_404() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.embed = Err(IpcError::new(
        "model_not_found",
        "model \"x\" not installed",
    ));
    let app = app(fake, 100);
    let t = token().await;
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/embeddings",
        Some(&t),
        r#"{"model":"x","input":"hi"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_openai_error(&v, "model_not_found");
    assert_eq!(v["error"]["param"], "model");
}

#[tokio::test]
async fn embeddings_bad_request_is_openai_400() {
    let app = app(FakeBackend::new(Ok(registry())), 100);
    let t = token().await;
    let (status, v, _) = send(&app, "POST", "/v1/embeddings", Some(&t), r#"{"model":"m"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_openai_error(&v, "invalid_request_error");
    assert_eq!(v["error"]["param"], "input");
}

#[tokio::test]
async fn embeddings_backend_down_is_openai_502() {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.embed = Err(IpcError::new(
        "pipe_unavailable",
        "wylde-ollama pipe missing",
    ));
    let app = app(fake, 100);
    let t = token().await;
    let (status, v, _) = send(
        &app,
        "POST",
        "/v1/embeddings",
        Some(&t),
        r#"{"model":"m","input":"hi"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_openai_error(&v, "upstream_unavailable");
    assert!(
        !v["error"]["message"].as_str().unwrap().contains("pipe"),
        "no internals leaked"
    );
}
