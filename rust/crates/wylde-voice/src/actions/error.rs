//! Shared error helpers for the Voice action surface.
//!
//! Stable codes — keep these in sync with `data/contracts/actions/wylde-voice.json`.
//! Every code below maps 1:1 onto an IPC error envelope downstream
//! callers parse out of the wire reply.

use serde_json::Value;
use wylde_shared::ipc::IpcError;

pub fn invalid_request(msg: impl Into<String>) -> IpcError {
    IpcError::new("invalid_request", msg)
}

pub fn audio_decode_failed(msg: impl Into<String>) -> IpcError {
    IpcError::new("audio_decode_failed", msg)
}

pub fn model_not_loaded(msg: impl Into<String>) -> IpcError {
    IpcError::new("model_not_loaded", msg)
}

pub fn inference_failed(msg: impl Into<String>) -> IpcError {
    IpcError::new("inference_failed", msg)
}

pub fn npu_unavailable(msg: impl Into<String>) -> IpcError {
    IpcError::new("npu_unavailable", msg)
}

/// Pull `field` out of a payload map as a non-empty string.
pub fn require_string(payload: &Value, field: &str) -> Result<String, IpcError> {
    let s = payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request(format!("payload.{field} is required (string)")))?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(invalid_request(format!("payload.{field} cannot be empty")));
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn require_string_rejects_missing_blank_and_wrong_type() {
        assert!(require_string(&json!({}), "audio_path").is_err());
        assert!(require_string(&json!({"audio_path": "  "}), "audio_path").is_err());
        assert!(require_string(&json!({"audio_path": 42}), "audio_path").is_err());
        assert_eq!(
            require_string(&json!({"audio_path": "x.wav"}), "audio_path").unwrap(),
            "x.wav"
        );
    }

    #[test]
    fn error_codes_match_contract() {
        assert_eq!(invalid_request("x").code, "invalid_request");
        assert_eq!(audio_decode_failed("x").code, "audio_decode_failed");
        assert_eq!(model_not_loaded("x").code, "model_not_loaded");
        assert_eq!(inference_failed("x").code, "inference_failed");
        assert_eq!(npu_unavailable("x").code, "npu_unavailable");
    }
}
