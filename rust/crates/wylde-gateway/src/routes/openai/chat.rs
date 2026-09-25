//! `POST /v1/chat/completions` over `ollama.chat_stream`.
//!
//! Both modes drive the streaming action, so a client disconnect always
//! drops the pipe and cancels generation (the model stays loaded:
//! `evict_on_cancel: false`). Non-streaming requests collect the frames
//! into one `chat.completion`; streaming requests get OpenAI SSE chunks,
//! tool-call deltas, an optional usage chunk and `data: [DONE]`.
//!
//! The first upstream frame is awaited before any response is committed,
//! so a failure there (unknown model, no VRAM, backend down) is a real
//! HTTP error, not an SSE error frame. When the request offered tools and
//! salvage is enabled for the model ([`super::salvage`]), streamed text is
//! held back until the reply is complete so text-form calls can be turned
//! into `tool_calls` before anything is sent.

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use futures::StreamExt;
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use super::backend::BackendStream;
use super::errors::OpenAiError;
use super::salvage::salvage;
use super::translate::{finish_reason, parse_chat, tool_calls_to_openai, usage, ChatRequest};
use super::{registry, sse, OpenAiState};

/// Identity shared by every chunk of one completion.
#[derive(Clone)]
pub(super) struct Meta {
    pub id: String,
    pub model: String,
    pub created: u64,
}

impl Meta {
    pub(super) fn new(prefix: &str, model: &str) -> Self {
        Self {
            id: format!("{prefix}-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_owned(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }
}

/// Running totals over Ollama chat frames.
#[derive(Default)]
struct Acc {
    content: String,
    tool_calls: Vec<Value>,
    done: bool,
    done_reason: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
}

impl Acc {
    /// Fold one frame in; returns its new text and new tool calls.
    fn absorb(&mut self, f: &Value) -> (String, Vec<Value>) {
        let text = f
            .pointer("/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        self.content.push_str(&text);
        let calls = f
            .pointer("/message/tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.tool_calls.extend(calls.iter().cloned());
        if f.get("done").and_then(Value::as_bool) == Some(true) {
            self.done = true;
            self.done_reason = f
                .get("done_reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            self.prompt_tokens = f
                .get("prompt_eval_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.completion_tokens = f.get("eval_count").and_then(Value::as_u64).unwrap_or(0);
        }
        (text, calls)
    }

    /// Final `(content, tool_calls)`, salvaging text-form calls when asked
    /// and the model made no structured ones.
    fn resolve(&self, req: &ChatRequest, salvage_on: bool) -> (String, Vec<Value>) {
        if salvage_on && self.tool_calls.is_empty() {
            if let Some((cleaned, calls)) = salvage(&self.content, &req.tool_names) {
                return (cleaned, calls);
            }
        }
        (self.content.clone(), self.tool_calls.clone())
    }
}

fn chunk(meta: &Meta, delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": meta.id, "object": "chat.completion.chunk", "created": meta.created,
        "model": meta.model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

/// `POST /v1/chat/completions`
pub async fn create(State(state): State<OpenAiState>, body: Bytes) -> Response {
    let view = registry::view(&state).await;
    let req = match parse_chat(&body, &view.aliases) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let salvage_on = !req.tool_names.is_empty() && state.salvage.enabled_for(&req.target);
    let mut upstream = state.backend.chat_stream(req.ollama.clone());
    let first = match upstream.next().await {
        Some(Ok(f)) => f,
        Some(Err(e)) => return OpenAiError::from_ipc(&e, &req.model).into_response(),
        None => return OpenAiError::upstream_unavailable().into_response(),
    };
    let meta = Meta::new("chatcmpl", &req.model);
    if req.stream {
        sse::response(stream(upstream, first, meta, req, salvage_on))
    } else {
        match collect(upstream, first, &req, salvage_on).await {
            Ok((content, calls, acc)) => {
                Json(completion(&meta, content, &calls, &acc)).into_response()
            }
            Err(e) => OpenAiError::from_ipc(&e, &req.model).into_response(),
        }
    }
}

async fn collect(
    mut upstream: BackendStream,
    first: Value,
    req: &ChatRequest,
    salvage_on: bool,
) -> Result<(String, Vec<Value>, Acc), IpcError> {
    let mut acc = Acc::default();
    acc.absorb(&first);
    while !acc.done {
        match upstream.next().await {
            Some(Ok(f)) => {
                acc.absorb(&f);
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }
    let (content, calls) = acc.resolve(req, salvage_on);
    Ok((content, calls, acc))
}

fn completion(meta: &Meta, content: String, calls: &[Value], acc: &Acc) -> Value {
    let mut message = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        let mut openai = tool_calls_to_openai(calls, 0);
        // Non-streamed tool calls carry no `index` (that's a delta field).
        for c in &mut openai {
            if let Some(m) = c.as_object_mut() {
                m.remove("index");
            }
        }
        message["tool_calls"] = Value::Array(openai);
        if content.is_empty() {
            message["content"] = Value::Null;
        }
    }
    json!({
        "id": meta.id, "object": "chat.completion", "created": meta.created, "model": meta.model,
        "choices": [{
            "index": 0, "message": message,
            "finish_reason": finish_reason(acc.done_reason.as_deref(), !calls.is_empty()),
        }],
        "usage": usage(acc.prompt_tokens, acc.completion_tokens),
    })
}

fn stream(
    mut upstream: BackendStream,
    first: Value,
    meta: Meta,
    req: ChatRequest,
    salvage_on: bool,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        yield Ok(sse::chunk(&chunk(&meta, json!({"role": "assistant", "content": ""}), None)));
        let mut acc = Acc::default();
        let mut sent_calls = 0usize;
        let mut frame = Some(Ok(first));
        loop {
            let f = match frame.take() {
                Some(Ok(f)) => f,
                Some(Err(e)) => {
                    yield Ok(sse::error(&OpenAiError::from_ipc(&e, &meta.model)));
                    return;
                }
                None => break,
            };
            let (text, calls) = acc.absorb(&f);
            if !salvage_on {
                if !text.is_empty() {
                    yield Ok(sse::chunk(&chunk(&meta, json!({"content": text}), None)));
                }
                if !calls.is_empty() {
                    let deltas = tool_calls_to_openai(&calls, sent_calls);
                    sent_calls += deltas.len();
                    yield Ok(sse::chunk(&chunk(&meta, json!({"tool_calls": deltas}), None)));
                }
            }
            if acc.done {
                break;
            }
            frame = upstream.next().await;
        }
        let has_calls = if salvage_on {
            let (content, calls) = acc.resolve(&req, true);
            if !content.is_empty() {
                yield Ok(sse::chunk(&chunk(&meta, json!({"content": content}), None)));
            }
            if !calls.is_empty() {
                let deltas = tool_calls_to_openai(&calls, 0);
                yield Ok(sse::chunk(&chunk(&meta, json!({"tool_calls": deltas}), None)));
            }
            !calls.is_empty()
        } else {
            sent_calls > 0
        };
        let finish = finish_reason(acc.done_reason.as_deref(), has_calls);
        yield Ok(sse::chunk(&chunk(&meta, json!({}), Some(finish))));
        if req.include_usage {
            yield Ok(sse::chunk(&json!({
                "id": meta.id, "object": "chat.completion.chunk", "created": meta.created,
                "model": meta.model, "choices": [],
                "usage": usage(acc.prompt_tokens, acc.completion_tokens),
            })));
        }
        yield Ok(sse::done());
    }
}
