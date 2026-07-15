//! `ollama.chat` (unary) + `ollama.chat_stream` (streaming).
//!
//! Both acquire a VRAM lease against the broker before the upstream
//! call lands (per design doc §3); both release it on every exit path
//! via the RAII guard.
//!
//! ## Cancellation propagation (design doc Q2)
//!
//! The open question is whether dropping a `reqwest` body stream
//! propagates "stop generating" upstream to Ollama. The cancellation
//! spike couldn't run in this session (no live Ollama daemon
//! reachable). Conservative default until the spike is run:
//!
//!   * Rely on body-stream drop first (free, costs nothing if it works).
//!   * On confirmed client-disconnect mid-stream, ALSO issue a
//!     fire-and-forget POST /api/generate {model, keep_alive: 0} to
//!     evict the model — this forces Ollama to drop whatever generation
//!     was in flight even if drop didn't propagate.
//!
//! If the spike later confirms drop suffices, the explicit-evict path
//! becomes redundant and can be deleted. Until then, both run.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reqwest::Method;
use serde_json::{json, Value};
use tokio::time::sleep;
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::actions::error::{
    excerpt, invalid_request, model_not_found_err, ollama_http_err, ollama_unreachable_err,
    require_string,
};
use crate::config::Config;
use crate::estimate::{estimate_vram_bytes, VramEstimate};
use crate::lease::{self, LeaseRequest, Priority};
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
/// Cancellation: if the client drops the IPC stream, the
/// `sender.send(...)` call below returns an error on the next chunk;
/// the handler bails, drops the lease, and (conservative default per
/// Q2) issues a fire-and-forget `/api/generate keep_alive=0` to ensure
/// Ollama stops generating even if reqwest body-drop didn't propagate.
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

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut cancelled = false;

    while let Some(chunk) = stream.next().await {
        // The sender's `closed()` future resolves when the consumer side
        // drops the receiver — which is what happens on IPC-stream
        // cancellation. We could `select!` on it here; but every
        // sender.send() also returns Err on a closed receiver, so we
        // observe the cancel naturally on the next emit. Cheaper.
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = sender.send(Err(ollama_unreachable_err(&e))).await; // wylde-check: discard-result-ok
                break;
            }
        };
        buf.extend_from_slice(&chunk);
        // Split on newline — Ollama emits one JSON object per line.
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = &line[..line.len() - 1]; // strip the trailing \n
            if line.is_empty() {
                continue;
            }
            let trimmed = trim_cr(line);
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Value>(trimmed) {
                Ok(v) => {
                    // Surface stream-level errors as Err frames.
                    if let Some(err) = v.get("error").and_then(Value::as_str) {
                        let _ = sender // wylde-check: discard-result-ok
                            .send(Err(IpcError::new(
                                "ollama_stream_error",
                                err.to_string(),
                            )))
                            .await;
                        // Terminate on stream error.
                        drop(lease_guard);
                        return;
                    }
                    if sender.send(Ok(v)).await.is_err() {
                        // Client dropped the stream — observe + bail.
                        cancelled = true;
                        break;
                    }
                }
                Err(_) => {
                    // Non-JSON line — Ollama doesn't emit these but be
                    // robust: skip silently rather than killing the stream.
                    continue;
                }
            }
        }
        if cancelled {
            break;
        }
    }

    // If there's a partial line left in the buffer at end-of-stream,
    // try to parse it (last line may not have a trailing newline).
    if !cancelled && !buf.is_empty() {
        let trimmed = trim_cr(&buf);
        if let Ok(v) = serde_json::from_slice::<Value>(trimmed) {
            let _ = sender.send(Ok(v)).await; // wylde-check: discard-result-ok
        }
    }

    // Cancellation cleanup: fire-and-forget eject (Q2 conservative
    // default). Best-effort; failure to eject doesn't matter — the
    // model will get evicted by Ollama's own keep_alive timer
    // eventually.
    if cancelled {
        let model_for_evict = model.clone();
        let up_clone = up.clone();
        tokio::spawn(async move {
            // Tiny delay so any in-flight final tokens land first.
            sleep(Duration::from_millis(200)).await;
            let body = json!({"model": model_for_evict, "keep_alive": 0});
            // Fire-and-forget: if Ollama is gone or busy, the model
            // will get evicted by its own keep_alive timer anyway.
            let _ = up_clone // wylde-check: discard-result-ok
                .client
                .post(format!("{}/api/generate", up_clone.base_url))
                .json(&body)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        });
    }

    drop(lease_guard);
}

fn trim_cr(line: &[u8]) -> &[u8] {
    if let Some(&last) = line.last() {
        if last == b'\r' {
            return &line[..line.len() - 1];
        }
    }
    line
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
