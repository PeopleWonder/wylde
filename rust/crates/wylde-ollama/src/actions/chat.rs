//! `ollama.chat` (unary) + `ollama.chat_stream` (streaming).
//!
//! Both acquire a VRAM lease against the broker before the upstream
//! call lands (per design doc §3); both release it on every exit path
//! via the RAII guard.
//!
//! The streaming relay and the evict-on-cancel policy (design doc Q2,
//! now behind the `evict_on_cancel` knob) live in
//! [`super::stream_relay`], shared with `ollama.generate_stream`.

use std::sync::Arc;

use reqwest::Method;
use serde_json::Value;
use wylde_shared::ipc::{Reply, StreamSender};

use crate::actions::error::{
    excerpt, invalid_request, model_not_found_err, ollama_http_err, ollama_unreachable_err,
    require_string,
};
use crate::actions::stream_relay::{self, RelayEnd};
use crate::config::Config;
use crate::estimate::{estimate_vram_bytes, VramEstimate};
use crate::lease::{self, LeaseRequest, Priority};
use crate::load_opts;
use crate::upstream::Upstream;

const BODY_EXCERPT_CAP: usize = 300;

/// `ollama.chat` — non-streaming. Acquires a lease, POSTs /api/chat
/// with stream=false, releases the lease on the way out.
pub async fn handle_chat(payload: Value, up: Arc<Upstream>) -> Reply {
    let cfg = Config::get();

    let model = match require_string(&payload, "model") {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };

    let messages = match payload.get("messages") {
        Some(v) if v.is_array() => v.clone(),
        _ => {
            return Reply::err(invalid_request("payload.messages is required (array)"));
        }
    };

    // Pass-through payload to upstream — every Ollama-known field flows
    // through without remapping, only `stream` is forced to false here.
    let mut body = payload.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), Value::Bool(false));
        // Force the messages field in case payload had it indirectly.
        obj.insert("messages".to_string(), messages);
        // Drop our pipe-only knobs before forwarding.
        obj.remove("priority");
    }
    load_opts::apply(&up, &model, &mut body).await;

    // Design §3 step 2: compute the VRAM footprint ourselves so the broker
    // gets a positive `bytes` (the Python broker has no estimator and would
    // reject a missing one with "bytes must be positive"). An absent model
    // surfaces as an actionable `model_not_found` here, before any reserve.
    let bytes_hint = match estimate_vram_bytes(&up, &model).await {
        VramEstimate::Bytes(b) => Some(b),
        VramEstimate::NotPulled => return Reply::err(model_not_found_err(&model)),
    };

    let priority = extract_priority(&payload);
    let lease_guard = match lease::acquire(LeaseRequest {
        model: model.clone(),
        bytes_hint,
        priority,
        nonce: None,
    })
    .await
    {
        Ok(l) => Some(l),
        Err(e) if e.code == "broker_unreachable" => {
            tracing::warn!(
                "wylde-ollama: chat broker unreachable, proceeding without lease: {}",
                e.message
            );
            None
        }
        Err(e) => return Reply::err(e),
    };

    let resp = match up
        .request(Method::POST, "/api/chat", Some(&body), cfg.chat_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if let Some(l) = lease_guard {
                l.release().await;
            }
            return Reply::err(ollama_unreachable_err(&e));
        }
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if let Some(l) = lease_guard {
            l.release().await;
        }
        return Reply::err(ollama_http_err(status, excerpt(&body, BODY_EXCERPT_CAP)));
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            if let Some(l) = lease_guard {
                l.release().await;
            }
            return Reply::err(ollama_unreachable_err(&e));
        }
    };

    if let Some(l) = lease_guard {
        l.release().await;
    }

    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(ollama_http_err(200, format!("decode failed: {e}"))),
    }
}

/// `ollama.chat_stream` — streaming. Acquires a lease, POSTs /api/chat
/// with stream=true, parses NDJSON lines from the response body, emits
/// one chunk per line to the [`StreamSender`].
///
/// Cancellation: if the client drops the IPC stream, the relay observes
/// it on the next emit; the handler drops the lease and, unless the
/// caller passed `evict_on_cancel: false`, fires the conservative
/// `keep_alive: 0` eviction (see [`super::stream_relay`]).
pub async fn handle_chat_stream(payload: Value, sender: StreamSender, up: Arc<Upstream>) {
    let model = match require_string(&payload, "model") {
        Ok(m) => m,
        Err(e) => {
            // Best-effort emit before bailing; if the client already
            // dropped the receiver there is nothing else to do.
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    let messages = match payload.get("messages") {
        Some(v) if v.is_array() => v.clone(),
        _ => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(invalid_request("payload.messages is required (array)")))
                .await;
            return;
        }
    };

    let mut body = payload.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), Value::Bool(true));
        obj.insert("messages".to_string(), messages);
        obj.remove("priority");
    }
    let evict_on_cancel = stream_relay::take_evict_flag(&mut body);
    load_opts::apply(&up, &model, &mut body).await;

    // Design §3 step 2: compute the footprint so the broker gets a positive
    // `bytes`; an absent model is surfaced as `model_not_found` up front.
    let bytes_hint = match estimate_vram_bytes(&up, &model).await {
        VramEstimate::Bytes(b) => Some(b),
        VramEstimate::NotPulled => {
            let _ = sender.send(Err(model_not_found_err(&model))).await; // wylde-check: discard-result-ok
            return;
        }
    };

    let priority = extract_priority(&payload);
    let lease_guard = match lease::acquire(LeaseRequest {
        model: model.clone(),
        bytes_hint,
        priority,
        nonce: None,
    })
    .await
    {
        Ok(l) => Some(l),
        Err(e) if e.code == "broker_unreachable" => {
            tracing::warn!(
                "wylde-ollama: chat_stream broker unreachable, proceeding without lease: {}",
                e.message
            );
            None
        }
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    // No per-call timeout on the chat_stream request — the per-chunk
    // IPC heartbeat is what bounds idle. A bounded timeout here would
    // cap useful streams (e.g. a 70B model that takes >2 min to finish).
    let resp = match up
        .client
        .post(format!("{}/api/chat", up.base_url))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = sender.send(Err(ollama_unreachable_err(&e))).await; // wylde-check: discard-result-ok
                                                                        // Lease drop on guard going out of scope.
            drop(lease_guard);
            return;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body_text = resp.text().await.unwrap_or_default();
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(ollama_http_err(
                status,
                excerpt(&body_text, BODY_EXCERPT_CAP),
            )))
            .await;
        drop(lease_guard);
        return;
    }

    let end = stream_relay::relay_ndjson(resp, &sender).await;
    if end == RelayEnd::Cancelled && evict_on_cancel {
        stream_relay::spawn_evict(up.clone(), model.clone());
    }
    drop(lease_guard);
}

fn extract_priority(payload: &Value) -> Priority {
    payload
        .get("priority")
        .and_then(Value::as_i64)
        .map(Priority::Explicit)
        .unwrap_or(Priority::Default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn fake_upstream() -> (MockServer, Arc<Upstream>) {
        let server = MockServer::start().await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    // These tests intentionally don't go through the broker — the
    // broker would need to be running as a process. We accept the
    // `lease::acquire` failure path (broker_unreachable) and verify
    // the action still completes correctly (the warn log triggers).

    #[tokio::test]
    async fn chat_passthrough_happy_path() {
        let (server, up) = fake_upstream().await;
        let envelope = json!({
            "message": {"role": "assistant", "content": "hello"},
            "done": true,
            "total_duration": 1_234_567,
            "prompt_eval_count": 5,
            "eval_count": 1
        });
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope.clone()))
            .mount(&server)
            .await;
        let r = handle_chat(
            json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]}),
            up,
        )
        .await;
        // The broker is unreachable in the test env; the handler
        // proceeds without a lease and returns the upstream payload.
        assert!(r.ok, "expected ok, got {r:?}");
        assert_eq!(r.data, envelope);
    }

    /// The pass-through contract the harness's constrained-decoding
    /// plumbing (`turn/reasoning/constrained.rs`) relies on: an Ollama
    /// `format` schema on the IPC payload reaches POST /api/chat
    /// unmodified (while pipe-only knobs like `priority` are stripped).
    /// The mock only matches when the upstream body carries the schema —
    /// an `ok` reply proves the field survived the hop.
    #[tokio::test]
    async fn chat_forwards_format_schema_upstream() {
        let (server, up) = fake_upstream().await;
        let schema = json!({"type": "object", "required": ["goal"]});
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(wiremock::matchers::body_partial_json(
                json!({"format": schema}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "{}"},
                "done": true
            })))
            .mount(&server)
            .await;
        let r = handle_chat(
            json!({
                "model": "qwen",
                "messages": [{"role": "user", "content": "hi"}],
                "format": schema,
                "priority": "high"
            }),
            up,
        )
        .await;
        assert!(r.ok, "format-bearing body must match upstream: {r:?}");
    }

    /// `pin_load_options: true` pins `num_ctx` to the resident model's
    /// context (so the request can't trigger a reload) and the knob itself
    /// never reaches Ollama.
    #[tokio::test]
    async fn chat_pins_load_options_when_asked() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "qwen", "context_length": 16384}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(wiremock::matchers::body_partial_json(
                json!({"options": {"num_ctx": 16384}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
            .mount(&server)
            .await;
        let r = handle_chat(
            json!({
                "model": "qwen",
                "messages": [{"role": "user", "content": "hi"}],
                "options": {"num_ctx": 4096},
                "pin_load_options": true
            }),
            up,
        )
        .await;
        assert!(r.ok, "pinned num_ctx must reach upstream: {r:?}");
        let sent = &server.received_requests().await.unwrap();
        let chat_body: Value = sent
            .iter()
            .find(|q| q.url.path() == "/api/chat")
            .map(|q| serde_json::from_slice(&q.body).unwrap())
            .unwrap();
        assert!(chat_body.get("pin_load_options").is_none());
    }

    #[tokio::test]
    async fn chat_requires_model_and_messages() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let r = handle_chat(json!({"messages": []}), up.clone()).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");

        let r = handle_chat(json!({"model": "x"}), up).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn chat_upstream_5xx_is_ollama_http() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let r = handle_chat(
            json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}),
            up,
        )
        .await;
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert_eq!(e.code, "ollama_http");
        assert_eq!(e.details.as_ref().unwrap()["status"], 500);
    }

    #[tokio::test]
    async fn chat_stream_emits_each_ndjson_line_as_chunk() {
        let (server, up) = fake_upstream().await;
        // 3 token chunks + final done.
        let ndjson = "\
            {\"message\":{\"role\":\"assistant\",\"content\":\"He\"},\"done\":false}\n\
            {\"message\":{\"role\":\"assistant\",\"content\":\"llo\"},\"done\":false}\n\
            {\"message\":{\"role\":\"assistant\",\"content\":\"!\"},\"done\":false}\n\
            {\"done\":true,\"eval_count\":3}\n";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(ndjson)
                    .insert_header("content-type", "application/x-ndjson"),
            )
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(16);
        handle_chat_stream(
            json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]}),
            tx,
            up,
        )
        .await;

        let mut chunks: Vec<Value> = Vec::new();
        while let Ok(item) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            match item {
                Some(Ok(v)) => chunks.push(v),
                Some(Err(e)) => panic!("stream errored: {e:?}"),
                None => break,
            }
        }
        assert_eq!(chunks.len(), 4, "expected 4 chunks, got {chunks:?}");
        assert_eq!(chunks[0]["message"]["content"], "He");
        assert_eq!(chunks[1]["message"]["content"], "llo");
        assert_eq!(chunks[2]["message"]["content"], "!");
        assert_eq!(chunks[3]["done"], true);
    }

    /// Drive a `chat_stream` whose client has already dropped the stream
    /// (so the first emit fails → cancelled) and count the `keep_alive: 0`
    /// evictions that reach Ollama.
    async fn evictions_after_cancel(extra: Value) -> usize {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "{\"message\":{\"content\":\"a\"},\"done\":false}\n{\"done\":true}\n",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "qwen", "keep_alive": 0}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
            .mount(&server)
            .await;
        let mut payload = json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]});
        if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        handle_chat_stream(payload, tx, up).await;
        // The eviction is spawned with a 200 ms delay.
        tokio::time::sleep(Duration::from_millis(700)).await;
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|q| q.url.path() == "/api/generate")
            .count()
    }

    #[tokio::test]
    async fn chat_stream_cancel_evicts_by_default() {
        assert_eq!(evictions_after_cancel(json!({})).await, 1);
    }

    #[tokio::test]
    async fn chat_stream_cancel_keeps_model_when_evict_on_cancel_false() {
        assert_eq!(
            evictions_after_cancel(json!({"evict_on_cancel": false})).await,
            0,
            "evict_on_cancel:false must not send keep_alive:0"
        );
    }

    #[tokio::test]
    async fn chat_stream_surfaces_inline_error() {
        let (server, up) = fake_upstream().await;
        let ndjson = "{\"error\":\"context exceeded\"}\n";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ndjson))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(4);
        handle_chat_stream(
            json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]}),
            tx,
            up,
        )
        .await;

        let first = rx.recv().await.expect("a frame");
        match first {
            Err(e) => assert_eq!(e.code, "ollama_stream_error"),
            Ok(v) => panic!("expected Err frame, got {v:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_upstream_5xx_is_first_error_frame() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(4);
        handle_chat_stream(
            json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]}),
            tx,
            up,
        )
        .await;
        let first = rx.recv().await.expect("a frame");
        match first {
            Err(e) => {
                assert_eq!(e.code, "ollama_http");
                assert_eq!(e.details.as_ref().unwrap()["status"], 502);
            }
            Ok(v) => panic!("expected Err frame, got {v:?}"),
        }
    }
}
