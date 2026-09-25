//! `ollama.generate` / `ollama.generate_stream` tests. The central property:
//! every exit path releases exactly the lease it acquired (proved with the
//! counting [`FakeLeaser`]).

use super::*;
use crate::lease::testing::{FakeLeaser, Outcome};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mock Ollama with model `m` on disk (so the estimate resolves) and not
/// loaded.
async fn ollama() -> (MockServer, Arc<Upstream>) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models": [{"name": "m", "size": 1_000_000}]})),
        )
        .mount(&server)
        .await;
    let up = crate::upstream::for_test(&server.uri());
    (server, up)
}

fn granting() -> Arc<FakeLeaser> {
    FakeLeaser::new(Outcome::Grant { spilled: false })
}

/// The one body the mock saw on POST /api/generate (the first, when a
/// cancel also posted an eviction).
async fn generate_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.unwrap();
    let q = reqs
        .iter()
        .find(|q| q.url.path() == "/api/generate")
        .expect("a POST /api/generate");
    serde_json::from_slice(&q.body).unwrap()
}

async fn drain(mut rx: mpsc::Receiver<Result<Value, IpcError>>) -> Vec<Result<Value, IpcError>> {
    let mut out = Vec::new();
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        out.push(item);
    }
    out
}

// ── unary ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn generate_forwards_fim_fields_and_strips_knobs() {
    let (server, up) = ollama().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"response": "a + b", "done": true})),
        )
        .mount(&server)
        .await;
    let leaser = granting();
    let r = handle_generate(
        json!({
            "model": "m", "prompt": "<|fim_prefix|>x", "suffix": "}", "raw": true,
            "options": {"temperature": 0.1, "stop": ["<|endoftext|>"]},
            "priority": 9, "fim": true, "evict_on_cancel": false, "pin_load_options": true
        }),
        up,
        leaser.clone(),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data["response"], "a + b");
    let sent = generate_body(&server).await;
    assert_eq!(sent["suffix"], "}");
    assert_eq!(sent["raw"], true);
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["options"]["temperature"], 0.1);
    for knob in ["priority", "fim", "evict_on_cancel", "pin_load_options"] {
        assert!(sent.get(knob).is_none(), "{knob} must not reach Ollama");
    }
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
    assert_eq!(leaser.priorities(), vec![9], "explicit priority wins");
}

#[tokio::test]
async fn generate_requires_model_and_string_prompt() {
    let (_s, up) = ollama().await;
    let leaser = granting();
    for payload in [
        json!({"prompt": "x"}),
        json!({"model": "m"}),
        json!({"model": "m", "prompt": 3}),
    ] {
        let r = handle_generate(payload, up.clone(), leaser.clone()).await;
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }
    assert_eq!(leaser.acquired(), 0, "no lease before validation passes");
}

#[tokio::test]
async fn generate_releases_lease_on_upstream_5xx() {
    let (server, up) = ollama().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let leaser = granting();
    let r = handle_generate(json!({"model": "m", "prompt": "hi"}), up, leaser.clone()).await;
    assert_eq!(r.error.unwrap().code, "ollama_http");
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
}

#[tokio::test]
async fn generate_releases_lease_on_undecodable_body() {
    let (server, up) = ollama().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let leaser = granting();
    let r = handle_generate(json!({"model": "m", "prompt": "hi"}), up, leaser.clone()).await;
    assert_eq!(r.error.unwrap().code, "ollama_http");
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
}

#[tokio::test]
async fn generate_releases_lease_when_ollama_unreachable() {
    // /api/ps and /api/tags fail → estimate falls back to a default, so the
    // lease is still taken; the POST then fails to connect.
    let up = crate::upstream::for_test("http://127.0.0.1:1");
    let leaser = granting();
    let r = handle_generate(json!({"model": "m", "prompt": "hi"}), up, leaser.clone()).await;
    assert_eq!(r.error.unwrap().code, "ollama_unreachable");
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
}

#[tokio::test]
async fn generate_fim_refusal_never_calls_generate() {
    let (server, up) = ollama().await;
    let leaser = FakeLeaser::new(Outcome::Grant { spilled: true });
    let r = handle_generate(
        json!({"model": "m", "prompt": "x", "fim": true}),
        up,
        leaser.clone(),
    )
    .await;
    assert_eq!(r.error.unwrap().code, "insufficient_vram");
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
    let posted = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|q| q.url.path() == "/api/generate");
    assert!(!posted, "a refused FIM call must not touch /api/generate");
}

// ── streaming ──────────────────────────────────────────────────────────

#[tokio::test]
async fn generate_stream_relays_chunks_and_releases_lease() {
    let (server, up) = ollama().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "{\"response\":\"a\",\"done\":false}\n{\"response\":\"b\",\"done\":false}\n{\"done\":true}\n",
        ))
        .mount(&server)
        .await;
    let leaser = granting();
    let (tx, rx) = mpsc::channel(16);
    handle_generate_stream(
        json!({"model": "m", "prompt": "hi"}),
        tx,
        up,
        leaser.clone(),
    )
    .await;
    let frames = drain(rx).await;
    let texts: Vec<Value> = frames.iter().map(|f| f.as_ref().unwrap().clone()).collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0]["response"], "a");
    assert_eq!(texts[2]["done"], true);
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
}

#[tokio::test]
async fn generate_stream_releases_lease_on_5xx_and_inline_error() {
    for (status, body, want) in [
        (502, "bad gateway", "ollama_http"),
        (
            200,
            "{\"error\":\"context exceeded\"}\n",
            "ollama_stream_error",
        ),
    ] {
        let (server, up) = ollama().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        let leaser = granting();
        let (tx, rx) = mpsc::channel(4);
        handle_generate_stream(
            json!({"model": "m", "prompt": "hi"}),
            tx,
            up,
            leaser.clone(),
        )
        .await;
        let frames = drain(rx).await;
        assert_eq!(frames[0].as_ref().unwrap_err().code, want);
        assert_eq!((leaser.acquired(), leaser.released()), (1, 1), "{want}");
    }
}

#[tokio::test]
async fn generate_stream_admission_error_is_first_frame() {
    let (_s, up) = ollama().await;
    let leaser = FakeLeaser::new(Outcome::Fail("vram_admission_denied"));
    let (tx, rx) = mpsc::channel(4);
    handle_generate_stream(
        json!({"model": "m", "prompt": "hi"}),
        tx,
        up,
        leaser.clone(),
    )
    .await;
    let frames = drain(rx).await;
    assert_eq!(
        frames[0].as_ref().unwrap_err().code,
        "vram_admission_denied"
    );
    assert_eq!(leaser.released(), 0);
}

/// Cancel a `generate_stream` (client already gone) and count the
/// `keep_alive: 0` evictions Ollama receives.
async fn evictions_after_cancel(extra: Value) -> (usize, Arc<FakeLeaser>) {
    let (server, up) = ollama().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{\"response\":\"a\",\"done\":false}\n{\"done\":true}\n"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .and(body_partial_json(json!({"model": "m", "keep_alive": 0})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
        .mount(&server)
        .await;
    let mut payload = json!({"model": "m", "prompt": "hi"});
    if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    let leaser = granting();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    handle_generate_stream(payload, tx, up, leaser.clone()).await;
    // The eviction is spawned with a 200 ms delay.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let evictions = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|q| {
            q.url.path() == "/api/generate"
                && serde_json::from_slice::<Value>(&q.body)
                    .is_ok_and(|b| b.get("keep_alive") == Some(&json!(0)))
        })
        .count();
    (evictions, leaser)
}

#[tokio::test]
async fn generate_stream_cancel_evicts_by_default() {
    let (evictions, leaser) = evictions_after_cancel(json!({})).await;
    assert_eq!(evictions, 1);
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
}

#[tokio::test]
async fn generate_stream_cancel_keeps_model_when_evict_on_cancel_false() {
    let (evictions, leaser) = evictions_after_cancel(json!({"evict_on_cancel": false})).await;
    assert_eq!(
        evictions, 0,
        "evict_on_cancel:false must not send keep_alive:0"
    );
    assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
}
