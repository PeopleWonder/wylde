//! `GET /v1/models` and `GET /v1/models/{id}`.
//!
//! Built from the harness model registry (`models.list`): chat models the
//! GUI shows (`kind: llm` with `chat_visible`) plus every embedding model
//! (`kind: embed`, which is never chat-visible but is what
//! `/v1/embeddings` needs). Speech, vision and wake-word models are left
//! out. If the harness is down, the list falls back to everything Ollama
//! has installed (`ollama.list_models`). Aliases whose target is present
//! are listed too, with `root` naming the real id.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use super::errors::OpenAiError;
use super::OpenAiState;

/// Registry entries exposed over `/v1`.
fn registry_ids(reply: &Value) -> Vec<String> {
    let models = reply.get("models").and_then(Value::as_array);
    models
        .into_iter()
        .flatten()
        .filter(|m| {
            let kind = m.get("kind").and_then(Value::as_str).unwrap_or("");
            let visible = m
                .get("chat_visible")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (kind == "llm" && visible) || kind == "embed"
        })
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// Model names from an Ollama `/api/tags` reply.
fn ollama_ids(reply: &Value) -> Vec<String> {
    let models = reply.get("models").and_then(Value::as_array);
    models
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn model_object(id: &str, root: Option<&str>) -> Value {
    let mut v = json!({"id": id, "object": "model", "created": 0, "owned_by": "wylde"});
    if let Some(r) = root {
        v["root"] = json!(r);
    }
    v
}

/// Every `/v1` model object: real ids first, then aliases whose target is
/// among them.
pub(super) async fn catalog(state: &OpenAiState) -> Result<Vec<Value>, OpenAiError> {
    let ids = match state.backend.registry_models().await {
        Ok(reply) => registry_ids(&reply),
        Err(e) => {
            tracing::warn!(
                "openai: models.list unavailable ({}), falling back to Ollama",
                e.code
            );
            let reply = state
                .backend
                .ollama_models()
                .await
                .map_err(|e| OpenAiError::from_ipc(&e, ""))?;
            ollama_ids(&reply)
        }
    };
    let mut out: Vec<Value> = ids.iter().map(|id| model_object(id, None)).collect();
    for (alias, target) in state.aliases.iter() {
        if ids.iter().any(|id| id == target) && !ids.iter().any(|id| id == alias) {
            out.push(model_object(alias, Some(target)));
        }
    }
    Ok(out)
}

/// `GET /v1/models`
pub async fn list(State(state): State<OpenAiState>) -> Response {
    match catalog(&state).await {
        Ok(data) => Json(json!({"object": "list", "data": data})).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /v1/models/{*id}` — ids may contain `/` (e.g. `hf.co/u/Repo:Q4`).
pub async fn get(State(state): State<OpenAiState>, Path(id): Path<String>) -> Response {
    match catalog(&state).await {
        Ok(data) => match data.into_iter().find(|m| m["id"] == id.as_str()) {
            Some(m) => Json(m).into_response(),
            None => OpenAiError::model_not_found(&id).into_response(),
        },
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_keep_visible_chat_and_all_embed_models() {
        let reply = json!({"models": [
            {"id": "coder", "kind": "llm", "chat_visible": true},
            {"id": "hidden-llm", "kind": "llm", "chat_visible": false},
            {"id": "nomic", "kind": "embed", "chat_visible": false},
            {"id": "whisper", "kind": "stt", "chat_visible": false},
        ]});
        assert_eq!(registry_ids(&reply), vec!["coder", "nomic"]);
    }

    #[test]
    fn ollama_ids_read_tag_names() {
        let reply = json!({"models": [{"name": "a:1"}, {"model": "no-name"}, {"name": "b"}]});
        assert_eq!(ollama_ids(&reply), vec!["a:1", "b"]);
    }
}
