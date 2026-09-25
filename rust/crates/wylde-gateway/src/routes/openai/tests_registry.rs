//! `/v1` aliases come only from the harness model registry (#348): every
//! route resolves through it, with installed-only / real-id-wins semantics;
//! with the harness unreachable there are no aliases and names pass through.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use super::backend::testing::FakeBackend;
use super::tests::{registry, registry_aliases, send, token};
use super::*;

const CODER: &str = "hf.co/u/Coder-GGUF:IQ3";

fn fake(aliases: Result<Value, IpcError>) -> FakeBackend {
    let mut f = FakeBackend::new(Ok(registry()));
    f.aliases = aliases;
    f.stream_frames = vec![Ok(json!({
        "message": {"content": "ok"}, "response": "ok", "done": true
    }))];
    f.embed = Ok(json!({"embeddings": [[0.5]], "prompt_eval_count": 1}));
    f
}

fn app(fake: Arc<FakeBackend>) -> Router {
    router_from(
        OpenAiState::new(fake, SalvagePolicy::parse("")),
        RateLimiter::new(1000),
    )
}

async fn model_ids(app: &Router, t: &str) -> Vec<(String, Option<String>)> {
    let (status, v, _) = send(app, "GET", "/v1/models", Some(t), "").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["id"].as_str().unwrap().to_owned(),
                m["root"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

async fn chat_model_sent(app: &Router, fake: &FakeBackend, t: &str, model: &str) -> String {
    let body = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
    let (status, v, _) = send(
        app,
        "POST",
        "/v1/chat/completions",
        Some(t),
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let sent = fake.payloads("chat_stream");
    sent.last().unwrap()["model"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn every_route_resolves_aliases_from_the_registry() {
    let fake = Arc::new(fake(registry_aliases(&[
        ("coder", CODER),
        ("embed", "nomic-embed-text:latest"),
    ])));
    let app = app(fake.clone());
    let t = token().await;

    let ids = model_ids(&app, &t).await;
    assert!(
        ids.contains(&("coder".into(), Some(CODER.into()))),
        "{ids:?}"
    );
    assert!(ids.contains(&("embed".into(), Some("nomic-embed-text:latest".into()))));

    assert_eq!(chat_model_sent(&app, &fake, &t, "coder").await, CODER);

    let (status, _, _) = send(
        &app,
        "POST",
        "/v1/completions",
        Some(&t),
        &json!({"model": "coder", "prompt": "x"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fake.payloads("generate_stream")[0]["model"], CODER);

    let (status, _, _) = send(
        &app,
        "POST",
        "/v1/embeddings",
        Some(&t),
        &json!({"model": "embed", "input": "hi"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fake.embed_calls.lock().unwrap()[0].0,
        "nomic-embed-text:latest"
    );
}

#[tokio::test]
async fn an_alias_to_a_model_that_is_not_installed_is_neither_listed_nor_resolved() {
    let fake = Arc::new(fake(registry_aliases(&[("ghost", "deleted:1")])));
    let app = app(fake.clone());
    let t = token().await;
    assert!(!model_ids(&app, &t)
        .await
        .iter()
        .any(|(id, _)| id == "ghost"));
    assert_eq!(
        chat_model_sent(&app, &fake, &t, "ghost").await,
        "ghost",
        "passed through"
    );
}

#[tokio::test]
async fn a_real_model_id_beats_an_alias_of_the_same_name() {
    // `whisper` is an installed model (kind stt) in the fake registry.
    let fake = Arc::new(fake(registry_aliases(&[("whisper", CODER)])));
    let app = app(fake.clone());
    let t = token().await;
    assert!(!model_ids(&app, &t)
        .await
        .iter()
        .any(|(id, r)| id == "whisper" && r.is_some()));
    assert_eq!(chat_model_sent(&app, &fake, &t, "whisper").await, "whisper");
}

#[tokio::test]
async fn with_the_harness_unreachable_names_pass_through_and_real_ids_work() {
    let mut f = FakeBackend::new(Err(IpcError::new("pipe_unavailable", "harness down")));
    f.aliases = Err(IpcError::new("pipe_unavailable", "harness down"));
    f.ollama = Ok(json!({"models": [{"name": CODER}]}));
    f.stream_frames = vec![Ok(json!({"message": {"content": "ok"}, "done": true}))];
    let fake = Arc::new(f);
    let app = app(fake.clone());
    let t = token().await;
    let ids = model_ids(&app, &t).await;
    assert_eq!(ids, vec![(CODER.to_owned(), None)], "no aliases listed");
    assert_eq!(
        chat_model_sent(&app, &fake, &t, "coder").await,
        "coder",
        "unresolved, unchanged"
    );
    assert_eq!(
        chat_model_sent(&app, &fake, &t, CODER).await,
        CODER,
        "real ids still work"
    );
}

#[tokio::test]
async fn the_registry_view_is_cached_between_requests() {
    let fake = Arc::new(fake(registry_aliases(&[("coder", CODER)])));
    let app = app(fake.clone());
    let t = token().await;
    model_ids(&app, &t).await;
    chat_model_sent(&app, &fake, &t, "coder").await;
    model_ids(&app, &t).await;
    assert_eq!(
        fake.registry_calls.load(Ordering::SeqCst),
        1,
        "one registry read within the TTL"
    );
}
