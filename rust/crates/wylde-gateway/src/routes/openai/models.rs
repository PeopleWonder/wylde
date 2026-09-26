//! `GET /v1/models` and `GET /v1/models/{id}`.
//!
//! Built from the harness model registry (`models.list`): chat models the
//! GUI shows (`kind: llm` with `chat_visible`) plus every embedding model
//! (`kind: embed`, which is never chat-visible but is what
//! `/v1/embeddings` needs). Speech, vision and wake-word models are left
//! out. If the harness is down, the list falls back to everything Ollama
//! has installed (`ollama.list_models`). Effective aliases from the model
//! registry (see [`super::registry`]) are listed too, with `root` naming
//! the real id.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use super::errors::OpenAiError;
use super::{registry, OpenAiState};

fn model_object(id: &str, root: Option<&str>) -> Value {
    let mut v = json!({"id": id, "object": "model", "created": 0, "owned_by": "wylde"});
    if let Some(r) = root {
        v["root"] = json!(r);
    }
    v
}

/// Every `/v1` model object: real ids first, then the effective aliases
/// (target installed, not shadowed by a real id) whose target is listed.
pub(super) async fn catalog(state: &OpenAiState) -> Result<Vec<Value>, OpenAiError> {
    let view = registry::view(state).await;
    if let Some(e) = &view.catalog_error {
        return Err(e.clone());
    }
    let mut out: Vec<Value> = view
        .listed
        .iter()
        .map(|id| model_object(id, None))
        .collect();
    for (alias, target) in view.aliases.iter() {
        if view.listed.iter().any(|id| id == target) {
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
