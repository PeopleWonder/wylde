//! `ollama.generate` (unary) + `ollama.generate_stream` (streaming).
//!
//! `POST /api/generate` behind a VRAM lease: the backend for the gateway's
//! OpenAI `/v1/completions` and FIM autocomplete (#342). The payload is
//! passed through to Ollama (`prompt`, `suffix`, `raw`, `options`,
//! `system`, `template`, `format`, `images`, `keep_alive`, …) with only
//! `stream` forced. Pipe-only knobs are stripped before forwarding:
//!
//! * `priority` — explicit lease priority.
//! * `fim` — FIM admission (see [`super::admission`]).
//! * `pin_load_options` — pin `num_ctx` to the resident model (see
//!   [`crate::load_opts`]).
//! * `evict_on_cancel` — `generate_stream` only (see
//!   [`super::stream_relay`]).
//!
//! The lease is held in a guard that is dropped on every exit path, which
//! releases it.

use std::sync::Arc;

use reqwest::Method;
use serde_json::Value;
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::actions::admission::{self, take_fim};
use crate::actions::error::{
    excerpt, invalid_request, ollama_http_err, ollama_unreachable_err, require_string,
};
use crate::actions::stream_relay::{self, RelayEnd};
use crate::config::Config;
use crate::lease::Leaser;
use crate::load_opts;
use crate::upstream::Upstream;

const BODY_EXCERPT_CAP: usize = 300;

/// A validated generate request, ready to forward.
struct Prepared {
    model: String,
    body: Value,
    fim: bool,
    evict_on_cancel: bool,
    residency: Option<load_opts::Residency>,
}

/// Validate the payload and build the upstream body: strip the pipe-only
/// knobs, force `stream`, and pin load options if asked.
async fn prepare(payload: &Value, stream: bool, up: &Upstream) -> Result<Prepared, IpcError> {
    let model = require_string(payload, "model")?;
    if !payload.get("prompt").is_some_and(Value::is_string) {
        return Err(invalid_request("payload.prompt is required (string)"));
    }
    let mut body = payload.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("priority");
        obj.insert("stream".to_owned(), Value::Bool(stream));
    }
    let fim = take_fim(&mut body);
    let evict_on_cancel = stream_relay::take_evict_flag(&mut body);
    let residency = load_opts::apply(up, &model, &mut body).await;
    Ok(Prepared {
        model,
        body,
        fim,
        evict_on_cancel,
        residency,
    })
}

/// `ollama.generate` — non-streaming. Admits a lease, POSTs /api/generate
/// with `stream: false`, releases the lease on the way out.
pub async fn handle_generate(payload: Value, up: Arc<Upstream>, leaser: Arc<dyn Leaser>) -> Reply {
    let cfg = Config::get();
    let p = match prepare(&payload, false, &up).await {
        Ok(p) => p,
        Err(e) => return Reply::err(e),
    };
    let lease = match admission::admit(&up, leaser.as_ref(), &p.model, &payload, p.fim, p.residency)
        .await
    {
        Ok(l) => l,
        Err(e) => return Reply::err(e),
    };

    let resp = match up
        .request(
            Method::POST,
            "/api/generate",
            Some(&p.body),
            cfg.chat_timeout_s,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            drop(lease);
            return Reply::err(ollama_unreachable_err(&e));
        }
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        drop(lease);
        return Reply::err(ollama_http_err(status, excerpt(&text, BODY_EXCERPT_CAP)));
    }
    let bytes = resp.bytes().await;
    drop(lease);
    match bytes {
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) => Reply::ok(v),
            Err(e) => Reply::err(ollama_http_err(200, format!("decode failed: {e}"))),
        },
        Err(e) => Reply::err(ollama_unreachable_err(&e)),
    }
}

/// `ollama.generate_stream` — streaming. Admits a lease, POSTs
/// /api/generate with `stream: true`, and relays each NDJSON line as a
/// frame. On a client cancel the lease is dropped and, unless the payload
/// passed `evict_on_cancel: false`, the model is evicted (design doc Q2).
pub async fn handle_generate_stream(
    payload: Value,
    sender: StreamSender,
    up: Arc<Upstream>,
    leaser: Arc<dyn Leaser>,
) {
    let p = match prepare(&payload, true, &up).await {
        Ok(p) => p,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };
    let lease = match admission::admit(&up, leaser.as_ref(), &p.model, &payload, p.fim, p.residency)
        .await
    {
        Ok(l) => l,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    // No per-call timeout — as with chat_stream, a bounded timeout would
    // cap long generations; the IPC heartbeat bounds idle.
    let resp = match up
        .client
        .post(format!("{}/api/generate", up.base_url))
        .json(&p.body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            drop(lease);
            let _ = sender.send(Err(ollama_unreachable_err(&e))).await; // wylde-check: discard-result-ok
            return;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        drop(lease);
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(ollama_http_err(
                status,
                excerpt(&text, BODY_EXCERPT_CAP),
            )))
            .await;
        return;
    }

    let end = stream_relay::relay_ndjson(resp, &sender).await;
    drop(lease);
    if end == RelayEnd::Cancelled && p.evict_on_cancel {
        stream_relay::spawn_evict(up.clone(), p.model.clone());
    }
}

#[cfg(test)]
mod tests;
