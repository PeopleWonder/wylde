//! Shared error helpers for the Trainer action surface.
//!
//! Stable codes:
//!   * `invalid_request` — payload schema decode failed
//!   * `worker_unreachable` — Python worker pipe couldn't be reached
//!     (lifecycle didn't spawn it / pipe not bound / call timed out)
//!   * `worker_failed` — worker returned a structured error envelope

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

pub fn invalid_request(msg: impl Into<String>) -> IpcError {
    IpcError::new("invalid_request", msg)
}

pub fn worker_unreachable(msg: impl Into<String>) -> IpcError {
    IpcError::new("worker_unreachable", msg)
}

pub fn worker_failed(code: impl Into<String>, message: impl Into<String>) -> IpcError {
    let code = code.into();
    let message = message.into();
    let mut e = IpcError::new("worker_failed", message.clone());
    e.details = Some(json!({"worker_code": code, "worker_message": message}));
    e
}

/// Convenience: pull `field` out of a payload map as a non-empty string.
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

    #[test]
    fn require_string_rejects_missing_blank_and_wrong_type() {
        let p = json!({});
        assert!(require_string(&p, "image_path").is_err());
        let p = json!({"image_path": "   "});
        assert!(require_string(&p, "image_path").is_err());
        let p = json!({"image_path": 42});
        assert!(require_string(&p, "image_path").is_err());
        let p = json!({"image_path": "a.png"});
        assert_eq!(require_string(&p, "image_path").unwrap(), "a.png");
    }

    #[test]
    fn worker_failed_carries_structured_details() {
        let e = worker_failed("captioner_oom", "CUDA OOM at 8.2 GB");
        assert_eq!(e.code, "worker_failed");
        let det = e.details.as_ref().unwrap();
        assert_eq!(det["worker_code"], "captioner_oom");
        assert_eq!(det["worker_message"], "CUDA OOM at 8.2 GB");
    }
}
