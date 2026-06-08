//! Embedding bridge — turns text into vectors via wylde-ollama.
//!
//! Relocated from the harness `crate::memory::embeddings` (Slice 0b). Stays
//! thin: a retry / shape-validation / Matryoshka-truncation wrapper over the
//! `ollama.embed` IPC action exposed by the wylde-ollama service. The
//! workspace ingest + RAG-query paths are the only consumers here; the
//! harness keeps its own copy for the rest of its memory layer (long-term,
//! global RAG), so this is a deliberate per-service client, not a fork to
//! reconcile.
//!
//! ## Matryoshka truncation
//!
//! nomic-embed-text packs the most discriminative information in the leading
//! dimensions, so prefix-slicing to a smaller `EMBED_DIM` is a valid quality
//! / cost tradeoff. We re-normalise after the slice so cosine stays valid.
//!
//! ## Retry policy
//!
//! Three attempts with exponential backoff (500ms → 1s → 2s). Network / 5xx
//! / broker-flakiness retried; 404 (model missing) and 4xx (request-shape)
//! fail fast.

use std::time::Duration;

use serde_json::{json, Value};
use wylde_shared::ipc::{self, IpcError};

use crate::common::{embed_dim, embed_model, embed_native_dim};
use crate::config::Config;

const RETRY_ATTEMPTS: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 500;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// Backend returned 404 — embedding model not pulled. Non-retryable.
    #[error("backend has no model named {model:?} — pull it with: ollama pull {model} ({detail})")]
    ModelMissing { model: String, detail: String },

    /// Request-shape error (4xx other than 404, or invalid_request). Non-retryable.
    #[error("backend rejected embed request: {0}")]
    BadRequest(String),

    /// Transient — exhausted all retries.
    #[error("embedding failed after {RETRY_ATTEMPTS} attempts: {0}")]
    Transient(String),

    /// Response decoded but failed our shape / dim invariants.
    #[error("embedding response invalid: {0}")]
    InvalidResponse(String),
}

/// Embed a list of texts. One vector per input. Returns an empty vec when
/// called with no inputs (does not round-trip to the backend).
pub async fn embed(texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let expected = texts.len();
    let model = embed_model();
    let cfg = Config::get();
    let service = cfg.ollama_service.clone();
    let payload = json!({
        "model": model,
        "input": texts,
    });

    let mut last_err: Option<String> = None;
    for attempt in 0..RETRY_ATTEMPTS {
        match ipc::call_action(&service, "ollama.embed", payload.clone()).await {
            Ok(value) => return parse_embed_response(value, expected),
            Err(e) => {
                if let Some(stop) = classify_terminal(&e, &model) {
                    return Err(stop);
                }
                last_err = Some(format_ipc_err(&e));
                tracing::warn!(
                    "workspaces.embeddings: transient embed failure (attempt {}/{}): {}",
                    attempt + 1,
                    RETRY_ATTEMPTS,
                    e.message
                );
            }
        }
        if attempt < RETRY_ATTEMPTS - 1 {
            let delay_ms = RETRY_BASE_DELAY_MS * (1u64 << attempt);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    Err(EmbedError::Transient(
        last_err.unwrap_or_else(|| "no error captured".to_owned()),
    ))
}

/// Embed a single text. Returns the lone vector.
pub async fn embed_one(text: String) -> Result<Vec<f32>, EmbedError> {
    let mut all = embed(vec![text]).await?;
    all.pop()
        .ok_or_else(|| EmbedError::InvalidResponse("backend returned 0 embeddings".to_owned()))
}

/// Decode a `{"embeddings": [[...], ...]}` response into validated +
/// Matryoshka-truncated vectors. Pure — kept public for direct unit testing.
pub fn parse_embed_response(
    value: Value,
    expected_count: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let embeddings = value.get("embeddings").cloned().unwrap_or(Value::Null);
    let arr = match embeddings.as_array() {
        Some(a) => a.clone(),
        None => {
            return Err(EmbedError::InvalidResponse(format!(
                "missing 'embeddings' list (top-level keys: {:?})",
                top_level_keys(&value),
            )))
        }
    };
    if arr.len() != expected_count {
        return Err(EmbedError::InvalidResponse(format!(
            "count mismatch: requested {expected_count}, got {}",
            arr.len()
        )));
    }
    let native = embed_native_dim();
    let target = embed_dim();
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let vec_arr = item
            .as_array()
            .ok_or_else(|| EmbedError::InvalidResponse(format!("entry {idx} is not an array")))?;
        if vec_arr.len() != native {
            return Err(EmbedError::InvalidResponse(format!(
                "native dim mismatch: expected {native}d from model {:?}, got {}d",
                embed_model(),
                vec_arr.len()
            )));
        }
        let mut floats: Vec<f32> = Vec::with_capacity(vec_arr.len());
        for (vi, x) in vec_arr.iter().enumerate() {
            let f = x
                .as_f64()
                .ok_or_else(|| EmbedError::InvalidResponse(format!("entry {idx}[{vi}] is not a number")))?;
            floats.push(f as f32);
        }
        if target < native {
            truncate_normalize_in_place(&mut floats, target);
        }
        out.push(floats);
    }
    Ok(out)
}

/// L2-normalise a Matryoshka prefix in-place. Public for direct unit testing.
pub fn truncate_normalize_in_place(vec: &mut Vec<f32>, dim: usize) {
    if vec.len() > dim {
        vec.truncate(dim);
    }
    let norm_sq: f32 = vec.iter().map(|x| x * x).sum();
    if norm_sq > 0.0 {
        let inv = 1.0 / norm_sq.sqrt();
        for x in vec.iter_mut() {
            *x *= inv;
        }
    }
}

/// Decide whether the IPC error is terminal (non-retryable). `None` means
/// "transient, keep retrying".
fn classify_terminal(err: &IpcError, model: &str) -> Option<EmbedError> {
    match err.code.as_str() {
        "model_not_found" => Some(EmbedError::ModelMissing {
            model: model.to_owned(),
            detail: err.message.clone(),
        }),
        "invalid_request" => Some(EmbedError::BadRequest(format_ipc_err(err))),
        "ollama_http" => {
            let status = err
                .details
                .as_ref()
                .and_then(|d| d.get("status"))
                .and_then(Value::as_u64);
            match status {
                Some(404) => Some(EmbedError::ModelMissing {
                    model: model.to_owned(),
                    detail: err.message.clone(),
                }),
                Some(s) if (400..500).contains(&s) => Some(EmbedError::BadRequest(format_ipc_err(err))),
                _ => None,
            }
        }
        _ => None,
    }
}

fn format_ipc_err(err: &IpcError) -> String {
    format!("{}: {}", err.code, err.message)
}

fn top_level_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::TEST_ENV_LOCK;

    /// Snapshot + restore both EMBED dim env vars around a test, holding the
    /// process-wide lock so concurrent tests don't observe each other's pins.
    struct DimGuard {
        _g: std::sync::MutexGuard<'static, ()>,
        prev_native: Option<String>,
        prev_target: Option<String>,
    }

    impl DimGuard {
        fn pin(native: usize, target: usize) -> Self {
            let g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prev_native = std::env::var("WYLDE_EMBED_NATIVE_DIM").ok();
            let prev_target = std::env::var("WYLDE_EMBED_DIM").ok();
            std::env::set_var("WYLDE_EMBED_NATIVE_DIM", native.to_string());
            std::env::set_var("WYLDE_EMBED_DIM", target.to_string());
            DimGuard {
                _g: g,
                prev_native,
                prev_target,
            }
        }
    }

    impl Drop for DimGuard {
        fn drop(&mut self) {
            match &self.prev_native {
                Some(v) => std::env::set_var("WYLDE_EMBED_NATIVE_DIM", v),
                None => std::env::remove_var("WYLDE_EMBED_NATIVE_DIM"),
            }
            match &self.prev_target {
                Some(v) => std::env::set_var("WYLDE_EMBED_DIM", v),
                None => std::env::remove_var("WYLDE_EMBED_DIM"),
            }
        }
    }

    #[test]
    fn parse_extracts_vectors_matching_count_and_dim() {
        let _g = DimGuard::pin(3, 3);
        let v = json!({ "embeddings": [[0.1, 0.2, 0.3], [0.4, 0.5, 0.6]] });
        let out = parse_embed_response(v, 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.1_f32, 0.2, 0.3]);
        assert_eq!(out[1], vec![0.4_f32, 0.5, 0.6]);
    }

    #[test]
    fn parse_rejects_missing_embeddings_key() {
        let _g = DimGuard::pin(3, 3);
        let err = parse_embed_response(json!({"foo": []}), 1).unwrap_err();
        match err {
            EmbedError::InvalidResponse(m) => assert!(m.contains("missing 'embeddings'")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_count_mismatch() {
        let _g = DimGuard::pin(3, 3);
        let err = parse_embed_response(json!({"embeddings": [[0.0, 0.0, 0.0]]}), 2).unwrap_err();
        match err {
            EmbedError::InvalidResponse(m) => assert!(m.contains("count mismatch")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_truncates_and_normalises_when_target_lt_native() {
        let _g = DimGuard::pin(4, 2);
        let v = json!({"embeddings": [[3.0, 4.0, 999.0, 999.0]]});
        let out = parse_embed_response(v, 1).unwrap();
        assert_eq!(out[0].len(), 2);
        assert!((out[0][0] - 0.6).abs() < 1e-6);
        assert!((out[0][1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn classify_terminal_maps_model_not_found_to_model_missing() {
        let err = IpcError::new("model_not_found", "nomic missing");
        match classify_terminal(&err, "nomic-embed-text").unwrap() {
            EmbedError::ModelMissing { model, .. } => assert_eq!(model, "nomic-embed-text"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn classify_terminal_treats_unreachable_as_transient() {
        let err = IpcError::new("ollama_unreachable", "connect refused");
        assert!(classify_terminal(&err, "x").is_none());
    }

    #[tokio::test]
    async fn embed_empty_input_skips_ipc() {
        let out = embed(Vec::new()).await.unwrap();
        assert!(out.is_empty());
    }
}
