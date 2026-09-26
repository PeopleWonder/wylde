//! `POST /v1/completions`: plain completions and FIM autocomplete, over
//! `ollama.generate_stream`.
//!
//! A request with a `suffix` is FIM. If the model reports Ollama's `insert`
//! capability, `prompt` and `suffix` go through natively. Otherwise the FIM
//! prompt is rendered server-side from the model family's template
//! ([`super::fim`]) and sent with `raw: true`. The family's special tokens
//! are always added as stop sequences, whichever path is used. No template
//! and no native support means a 400 `fim_unsupported`. A prompt that is
//! already a rendered FIM prompt for the model's family (no `suffix`; how
//! Continue's OpenAI provider can send autocomplete) is FIM too: it goes
//! out `raw: true` with the family's stops, so Ollama never wraps it in the
//! chat template.
//!
//! FIM requests pass `fim: true` (FIM lease priority in `wylde-ollama`) and
//! take a ticket in the per-device FIM lane ([`super::lane`]): a newer FIM
//! request from the same device supersedes this one, which then drops its
//! upstream stream (cancelling generation, model kept loaded) and answers
//! 409 `request_superseded`, or ends its SSE stream with that error.

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use super::aliases::Aliases;
use super::backend::BackendStream;
use super::chat::Meta;
use super::errors::OpenAiError;
use super::fim::{template_for, FimTemplate};
use super::lane::FimTicket;
use super::translate::{finish_reason, stop_list, usage};
use super::{registry, sse, OpenAiState};
use crate::auth::Device;

/// A validated `/v1/completions` request.
struct CompletionRequest {
    model: String,
    target: String,
    prompt: String,
    suffix: Option<String>,
    stream: bool,
    include_usage: bool,
    options: Map<String, Value>,
}

fn bad(msg: &str, param: &str) -> OpenAiError {
    OpenAiError::invalid_request(msg, Some(param))
}

fn parse(body: &[u8], aliases: &Aliases) -> Result<CompletionRequest, OpenAiError> {
    let req: Value = serde_json::from_slice(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| OpenAiError::invalid_request("Request body must be a JSON object", None))?;
    let model = req
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| bad("'model' is required", "model"))?
        .to_owned();
    let prompt = match req.get("prompt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) if a.len() == 1 && a[0].is_string() => {
            a[0].as_str().unwrap_or("").to_owned()
        }
        _ => {
            return Err(bad(
                "'prompt' must be a string (or a one-element array)",
                "prompt",
            ))
        }
    };
    if req.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        return Err(bad("Only n=1 is supported", "n"));
    }
    let suffix = match req.get("suffix") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(bad("'suffix' must be a string", "suffix")),
    };
    let mut options = Map::new();
    for key in [
        "temperature",
        "top_p",
        "seed",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(v) = req.get(key).filter(|v| v.is_number()) {
            options.insert(key.to_owned(), v.clone());
        }
    }
    if let Some(v) = req.get("max_tokens").filter(|v| !v.is_null()) {
        let n = v
            .as_u64()
            .ok_or_else(|| bad("max_tokens must be a positive integer", "max_tokens"))?;
        options.insert("num_predict".to_owned(), json!(n));
    }
    if let Some(stop) = stop_list(req.get("stop"))? {
        options.insert("stop".to_owned(), json!(stop));
    }
    Ok(CompletionRequest {
        target: aliases.resolve(&model).to_owned(),
        model,
        prompt,
        suffix,
        stream: req.get("stream").and_then(Value::as_bool).unwrap_or(false),
        include_usage: req
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        options,
    })
}

/// Whether `model` supports Ollama's native `insert` (cached per model;
/// a failed lookup counts as "no" and isn't cached).
async fn supports_insert(state: &OpenAiState, model: &str) -> bool {
    if let Some(&known) = state.insert_cache.lock().expect("insert cache").get(model) {
        return known;
    }
    match state.backend.show(model.to_owned()).await {
        Ok(show) => {
            let insert = show
                .get("capabilities")
                .and_then(Value::as_array)
                .is_some_and(|caps| caps.iter().any(|c| c == "insert"));
            state
                .insert_cache
                .lock()
                .expect("insert cache")
                .insert(model.to_owned(), insert);
            insert
        }
        Err(_) => false,
    }
}

/// Append `template`'s stop tokens to the caller's `stop` list (caller's
/// first, no duplicates).
fn add_template_stops(options: &mut Map<String, Value>, template: &FimTemplate) {
    let mut stops: Vec<Value> = options
        .get("stop")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for s in template.stops {
        if !stops.iter().any(|x| x == s) {
            stops.push(json!(s));
        }
    }
    options.insert("stop".to_owned(), Value::Array(stops));
}

/// Build the `ollama.generate_stream` payload, choosing the FIM path.
/// Returns the payload and whether the request is FIM.
async fn upstream_payload(
    state: &OpenAiState,
    req: &CompletionRequest,
) -> Result<(Value, bool), OpenAiError> {
    let mut options = req.options.clone();
    let mut payload = json!({
        "model": req.target,
        "prompt": req.prompt,
        "stream": true,
        "evict_on_cancel": false,
        "pin_load_options": true,
    });
    let template = template_for(&req.target);
    let fim = if let Some(suffix) = &req.suffix {
        if supports_insert(state, &req.target).await {
            payload["suffix"] = json!(suffix);
        } else if let Some(t) = template {
            payload["prompt"] = json!(t.render(&req.prompt, suffix));
            payload["raw"] = json!(true);
        } else {
            return Err(OpenAiError::fim_unsupported(&req.model));
        }
        true
    } else if template.is_some_and(|t| t.is_rendered(&req.prompt)) {
        // Already rendered by the client: send it untouched.
        payload["raw"] = json!(true);
        true
    } else {
        false
    };
    if fim {
        if let Some(t) = template {
            add_template_stops(&mut options, t);
        }
        payload["fim"] = json!(true);
    }
    payload["options"] = Value::Object(options);
    Ok((payload, fim))
}

/// Resolves when `ticket` is superseded; never, for a non-FIM request.
async fn superseded(ticket: &mut Option<FimTicket>) {
    match ticket {
        Some(t) => t.superseded().await,
        None => std::future::pending().await,
    }
}

/// The next upstream frame, or `None` once this FIM request is superseded.
async fn next_frame(
    upstream: &mut BackendStream,
    ticket: &mut Option<FimTicket>,
) -> Option<Option<Result<Value, IpcError>>> {
    tokio::select! {
        biased;
        _ = superseded(ticket) => None,
        f = upstream.next() => Some(f),
    }
}

/// `POST /v1/completions`
pub async fn create(
    State(state): State<OpenAiState>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Response {
    let view = registry::view(&state).await;
    let req = match parse(&body, &view.aliases) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let (payload, fim) = match upstream_payload(&state, &req).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let mut ticket = fim.then(|| state.lane.begin(&device.device_id));
    let mut upstream = state.backend.generate_stream(payload);
    let first = match next_frame(&mut upstream, &mut ticket).await {
        None => return OpenAiError::request_superseded().into_response(),
        Some(Some(Ok(f))) => f,
        Some(Some(Err(e))) => return OpenAiError::from_ipc(&e, &req.model).into_response(),
        Some(None) => return OpenAiError::upstream_unavailable().into_response(),
    };
    let meta = Meta::new("cmpl", &req.model);
    if req.stream {
        return sse::response(stream(upstream, ticket, first, meta, req.include_usage));
    }
    let mut text = String::new();
    let mut frame = first;
    loop {
        text.push_str(frame.get("response").and_then(Value::as_str).unwrap_or(""));
        if frame.get("done").and_then(Value::as_bool) == Some(true) {
            break;
        }
        frame = match next_frame(&mut upstream, &mut ticket).await {
            None => return OpenAiError::request_superseded().into_response(),
            Some(Some(Ok(f))) => f,
            Some(Some(Err(e))) => return OpenAiError::from_ipc(&e, &req.model).into_response(),
            Some(None) => break,
        };
    }
    Json(json!({
        "id": meta.id, "object": "text_completion", "created": meta.created, "model": meta.model,
        "choices": [{
            "index": 0, "text": text, "logprobs": null,
            "finish_reason": finish_reason(frame.get("done_reason").and_then(Value::as_str), false),
        }],
        "usage": frame_usage(&frame),
    }))
    .into_response()
}

fn frame_usage(f: &Value) -> Value {
    usage(
        f.get("prompt_eval_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        f.get("eval_count").and_then(Value::as_u64).unwrap_or(0),
    )
}

fn stream(
    mut upstream: BackendStream,
    mut ticket: Option<FimTicket>,
    first: Value,
    meta: Meta,
    include_usage: bool,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let chunk = move |meta: &Meta, text: &str, finish: Option<&str>| {
        json!({
            "id": meta.id, "object": "text_completion", "created": meta.created, "model": meta.model,
            "choices": [{"index": 0, "text": text, "logprobs": null, "finish_reason": finish}],
        })
    };
    async_stream::stream! {
        let mut frame = first;
        loop {
            let text = frame.get("response").and_then(Value::as_str).unwrap_or("");
            if !text.is_empty() {
                yield Ok(sse::chunk(&chunk(&meta, text, None)));
            }
            if frame.get("done").and_then(Value::as_bool) == Some(true) {
                break;
            }
            frame = match next_frame(&mut upstream, &mut ticket).await {
                None => {
                    // Dropping `upstream` when this ends cancels generation.
                    yield Ok(sse::error(&OpenAiError::request_superseded()));
                    return;
                }
                Some(Some(Ok(f))) => f,
                Some(Some(Err(e))) => {
                    yield Ok(sse::error(&OpenAiError::from_ipc(&e, &meta.model)));
                    return;
                }
                Some(None) => json!({"done": true}),
            };
        }
        let finish = finish_reason(frame.get("done_reason").and_then(Value::as_str), false);
        yield Ok(sse::chunk(&chunk(&meta, "", Some(finish))));
        if include_usage {
            yield Ok(sse::chunk(&json!({
                "id": meta.id, "object": "text_completion", "created": meta.created,
                "model": meta.model, "choices": [], "usage": frame_usage(&frame),
            })));
        }
        yield Ok(sse::done());
    }
}
