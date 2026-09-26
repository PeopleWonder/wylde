//! `POST /v1/embeddings` via the leased `ollama.embed`.
//!
//! Supports `model` (an id or alias), `input` as a string or an array of
//! strings, and `encoding_format: "float"` (the default). Token-array
//! input and `base64` output are rejected with a 400. `dimensions` and
//! `user` are accepted and ignored.

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use super::errors::OpenAiError;
use super::{registry, OpenAiState};

/// A validated request: `(model, input)`.
fn parse(body: &[u8]) -> Result<(String, Value), OpenAiError> {
    let req: Value = serde_json::from_slice(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| OpenAiError::invalid_request("Request body must be a JSON object", None))?;
    let model = req
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| OpenAiError::invalid_request("'model' is required", Some("model")))?;
    let input = match req.get("input") {
        Some(Value::String(s)) if !s.is_empty() => Value::String(s.clone()),
        Some(Value::Array(items))
            if !items.is_empty()
                && items
                    .iter()
                    .all(|i| i.as_str().is_some_and(|s| !s.is_empty())) =>
        {
            Value::Array(items.clone())
        }
        Some(Value::Array(items)) if items.iter().any(|i| i.is_number() || i.is_array()) => {
            return Err(OpenAiError::invalid_request(
                "Token-array input is not supported; send text",
                Some("input"),
            ))
        }
        _ => {
            return Err(OpenAiError::invalid_request(
                "'input' must be a non-empty string or array of non-empty strings",
                Some("input"),
            ))
        }
    };
    match req.get("encoding_format").and_then(Value::as_str) {
        None | Some("float") => {}
        Some(_) => {
            return Err(OpenAiError::invalid_request(
                "Only encoding_format 'float' is supported",
                Some("encoding_format"),
            ))
        }
    }
    Ok((model.to_owned(), input))
}

/// Reshape an Ollama `/api/embed` reply into OpenAI's list form.
fn to_openai(reply: &Value, model: &str) -> Result<Value, OpenAiError> {
    let vectors = reply
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(OpenAiError::internal)?;
    let data: Vec<Value> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"object": "embedding", "index": i, "embedding": v}))
        .collect();
    let tokens = reply
        .get("prompt_eval_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(json!({
        "object": "list",
        "data": data,
        "model": model,
        "usage": {"prompt_tokens": tokens, "total_tokens": tokens},
    }))
}

/// `POST /v1/embeddings`
pub async fn create(State(state): State<OpenAiState>, body: Bytes) -> Response {
    let (model, input) = match parse(&body) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let target = registry::view(&state)
        .await
        .aliases
        .resolve(&model)
        .to_owned();
    let reply = match state.backend.embed(target, input).await {
        Ok(r) => r,
        Err(e) => return OpenAiError::from_ipc(&e, &model).into_response(),
    };
    match to_openai(&reply, &model) {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_param(body: &str) -> Option<String> {
        parse(body.as_bytes()).unwrap_err().param
    }

    #[test]
    fn parse_accepts_string_and_string_array() {
        assert!(parse(br#"{"model":"m","input":"hi"}"#).is_ok());
        assert!(parse(br#"{"model":"m","input":["a","b"],"encoding_format":"float"}"#).is_ok());
    }

    #[test]
    fn parse_rejects_bad_fields_naming_the_param() {
        assert_eq!(err_param(r#"{"input":"hi"}"#).as_deref(), Some("model"));
        assert_eq!(err_param(r#"{"model":"m"}"#).as_deref(), Some("input"));
        assert_eq!(
            err_param(r#"{"model":"m","input":[]}"#).as_deref(),
            Some("input")
        );
        assert_eq!(
            err_param(r#"{"model":"m","input":[1,2]}"#).as_deref(),
            Some("input")
        );
        assert_eq!(
            err_param(r#"{"model":"m","input":"x","encoding_format":"base64"}"#).as_deref(),
            Some("encoding_format")
        );
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn to_openai_indexes_vectors_and_reports_usage() {
        let v = to_openai(
            &json!({"embeddings": [[0.1, 0.2], [0.3]], "prompt_eval_count": 4}),
            "embed",
        )
        .unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][1]["index"], 1);
        assert_eq!(v["data"][1]["embedding"], json!([0.3]));
        assert_eq!(v["usage"]["total_tokens"], 4);
        assert_eq!(v["model"], "embed");
    }
}
