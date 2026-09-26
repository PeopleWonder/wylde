//! Router tests for `POST /v1/completions` (plain + FIM) over a fake backend.

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wylde_shared::ipc::IpcError;

use super::backend::testing::FakeBackend;
use super::tests::{assert_openai_error, registry, send, token};
use super::*;

fn app(fake: Arc<FakeBackend>) -> Router {
    router_from(
        OpenAiState::new(fake, SalvagePolicy::parse("")),
        RateLimiter::new(1000),
    )
}

fn gen_frames() -> Vec<Result<Value, IpcError>> {
    vec![
        Ok(json!({"response": "a + ", "done": false})),
        Ok(json!({"response": "b", "done": false})),
        Ok(
            json!({"response": "", "done": true, "done_reason": "stop", "prompt_eval_count": 12, "eval_count": 3}),
        ),
    ]
}

fn fake_with(show_caps: Value) -> FakeBackend {
    let mut fake = FakeBackend::new(Ok(registry()));
    fake.show = Ok(json!({"capabilities": show_caps}));
    fake.stream_frames = gen_frames();
    fake
}

async fn complete(app: &Router, token: &str, body: Value) -> (StatusCode, Value) {
    let (s, v, _) = send(
        app,
        "POST",
        "/v1/completions",
        Some(token),
        &body.to_string(),
    )
    .await;
    (s, v)
}

#[tokio::test]
async fn fim_without_native_insert_renders_the_family_template_raw_with_stops() {
    let fake = Arc::new(fake_with(json!(["completion"])));
    let app = app(fake.clone());
    let t = token().await;
    let (status, v) = complete(
        &app,
        &t,
        json!({
            "model": "qwen2.5-coder:3b-base", "prompt": "def add(a, b):\n    ", "suffix": "\n",
            "max_tokens": 32, "temperature": 0.1, "stop": ["\n\n"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["object"], "text_completion");
    assert_eq!(v["choices"][0]["text"], "a + b");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 12);
    let sent = &fake.payloads("generate_stream")[0];
    assert_eq!(
        sent["prompt"],
        "<|fim_prefix|>def add(a, b):\n    <|fim_suffix|>\n<|fim_middle|>"
    );
    assert_eq!(sent["raw"], true);
    assert!(sent.get("suffix").is_none());
    assert_eq!(sent["fim"], true);
    assert_eq!(sent["evict_on_cancel"], false);
    assert_eq!(sent["pin_load_options"], true);
    assert_eq!(sent["options"]["num_predict"], 32);
    let stops = sent["options"]["stop"].as_array().unwrap();
    assert_eq!(stops[0], "\n\n", "caller stops kept first");
    for s in [
        "<|endoftext|>",
        "<|fim_prefix|>",
        "<|fim_suffix|>",
        "<|fim_middle|>",
        "<|file_sep|>",
    ] {
        assert!(stops.contains(&json!(s)), "missing template stop {s}");
    }
}

#[tokio::test]
async fn fim_with_native_insert_sends_the_suffix_and_still_adds_stops() {
    let fake = Arc::new(fake_with(json!(["completion", "insert"])));
    let app = app(fake.clone());
    let t = token().await;
    let (status, _) = complete(
        &app,
        &t,
        json!({"model": "qwen2.5-coder:7b", "prompt": "fn f() {", "suffix": "}"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sent = &fake.payloads("generate_stream")[0];
    assert_eq!(sent["prompt"], "fn f() {");
    assert_eq!(sent["suffix"], "}");
    assert!(sent.get("raw").is_none());
    assert!(sent["options"]["stop"]
        .as_array()
        .unwrap()
        .contains(&json!("<|fim_middle|>")));
}

#[tokio::test]
async fn fim_on_a_model_without_insert_or_template_is_400() {
    let fake = Arc::new(fake_with(json!(["completion"])));
    let app = app(fake.clone());
    let t = token().await;
    let (status, v) = complete(
        &app,
        &t,
        json!({"model": "gemma4:12b", "prompt": "x", "suffix": "y"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_openai_error(&v, "fim_unsupported");
    assert_eq!(v["error"]["param"], "suffix");
    assert!(fake.payloads("generate_stream").is_empty());
}

#[tokio::test]
async fn plain_completion_is_not_fim() {
    let fake = Arc::new(fake_with(json!(["completion"])));
    let app = app(fake.clone());
    let t = token().await;
    let (status, v) = complete(
        &app,
        &t,
        json!({"model": "gemma4:12b", "prompt": ["Once upon"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let sent = &fake.payloads("generate_stream")[0];
    assert_eq!(sent["prompt"], "Once upon");
    assert!(sent.get("fim").is_none() && sent.get("raw").is_none());
}

/// Continue's OpenAI provider can send autocomplete as an already-rendered
/// FIM prompt with no `suffix`: it must go out raw (never wrapped in the
/// chat template), as FIM, with the family's stop tokens.
#[tokio::test]
async fn a_pre_rendered_fim_prompt_without_suffix_is_sent_raw_as_fim() {
    let fake = Arc::new(fake_with(json!(["completion"])));
    let app = app(fake.clone());
    let t = token().await;
    let prompt = "<|fim_prefix|>fn f() {<|fim_suffix|>}<|fim_middle|>";
    let (status, v) = complete(
        &app,
        &t,
        json!({"model": "qwen2.5-coder:7b", "prompt": prompt}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let sent = &fake.payloads("generate_stream")[0];
    assert_eq!(sent["prompt"], prompt, "sent untouched");
    assert_eq!(sent["raw"], true);
    assert_eq!(sent["fim"], true);
    assert!(sent["options"]["stop"]
        .as_array()
        .unwrap()
        .contains(&json!("<|fim_middle|>")));
}

#[tokio::test]
async fn stream_sends_text_chunks_finish_and_done() {
    let fake = Arc::new(fake_with(json!(["completion", "insert"])));
    let app = app(fake);
    let t = token().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("authorization", format!("Bearer {t}"))
                .body(Body::from(
                    json!({"model": "qwen2.5-coder:7b", "prompt": "x", "suffix": "y", "stream": true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw =
        String::from_utf8(to_bytes(resp.into_body(), 1 << 16).await.unwrap().to_vec()).unwrap();
    let datas: Vec<&str> = raw
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .collect();
    assert_eq!(datas.last(), Some(&"[DONE]"), "{raw}");
    let texts: Vec<Value> = datas[..datas.len() - 1]
        .iter()
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert_eq!(texts[0]["choices"][0]["text"], "a + ");
    assert_eq!(texts[1]["choices"][0]["text"], "b");
    assert_eq!(texts[2]["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn mid_stream_error_ends_the_sse_stream_without_done() {
    let mut fake = fake_with(json!(["completion", "insert"]));
    fake.stream_frames = vec![
        Ok(json!({"response": "a", "done": false})),
        Err(IpcError::new("ollama_unreachable", "reset")),
    ];
    let app = app(Arc::new(fake));
    let t = token().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("authorization", format!("Bearer {t}"))
                .body(Body::from(
                    json!({"model": "m", "prompt": "x", "stream": true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let raw =
        String::from_utf8(to_bytes(resp.into_body(), 1 << 16).await.unwrap().to_vec()).unwrap();
    assert!(!raw.contains("[DONE]"), "{raw}");
    assert!(raw.contains("\"upstream_unavailable\""), "{raw}");
}

#[tokio::test]
async fn a_newer_fim_request_from_the_same_device_supersedes_the_older_one() {
    let fake = Arc::new(fake_with(json!(["completion", "insert"])));
    fake.hang_next_stream.store(true, Ordering::SeqCst);
    let app = app(fake.clone());
    let t = token().await;
    let fim = json!({"model": "qwen2.5-coder:7b", "prompt": "x", "suffix": "y"});

    // The first request's upstream never answers.
    let first = {
        let (app, t, body) = (app.clone(), t.clone(), fim.clone());
        tokio::spawn(async move { complete(&app, &t, body).await })
    };
    for _ in 0..100 {
        if fake.payloads("generate_stream").len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fake.payloads("generate_stream").len(),
        1,
        "first request reached upstream"
    );

    // A newer FIM request from the same device wins…
    let (status, v) = complete(&app, &t, fim.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["choices"][0]["text"], "a + b");

    // …and the older one is told it was superseded, its stream dropped.
    let (status, v) = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("superseded request returns promptly")
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_openai_error(&v, "request_superseded");
    assert_eq!(
        fake.dropped_streams.load(Ordering::SeqCst),
        2,
        "both upstream streams released"
    );
}
