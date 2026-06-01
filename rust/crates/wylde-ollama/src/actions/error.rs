//! Shared error helpers — every action surfaces the same stable codes.
//!
//! Stable codes (per `docs/wylde-ollama-design.md §1a`):
//!   * `ollama_unreachable` — TCP/DNS/connect failure
//!   * `ollama_http`        — non-2xx from Ollama, details: {status, body_excerpt}
//!   * `model_not_found`    — 404 for show/delete/embed/eject when the model isn't installed
//!   * `invalid_request`    — payload schema decode failed
//!   * `pull_failed`        — pull stream ended without success
//!   * `vram_admission_denied` — broker said no
//!   * `broker_unreachable` — broker pipe couldn't be reached

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

pub fn invalid_request(msg: impl Into<String>) -> IpcError {
    IpcError::new("invalid_request", msg)
}

pub fn ollama_unreachable_err(e: &reqwest::Error) -> IpcError {
    IpcError::new("ollama_unreachable", format!("upstream: {e}"))
}

/// Map a non-2xx response to the standard envelope.
/// `status` is the HTTP status code; `body_excerpt` is the truncated
/// response body (up to ~300 chars per the Python convention).
pub fn ollama_http_err(status: u16, body_excerpt: String) -> IpcError {
    let mut e = IpcError::new(
        "ollama_http",
        format!("ollama returned {status}: {body_excerpt}"),
    );
    e.details = Some(json!({
        "status": status,
        "body_excerpt": body_excerpt,
    }));
    e
}

/// Map a 404 to `model_not_found` rather than the generic
/// `ollama_http`. Used by show/delete/embed/eject per the design doc.
pub fn model_not_found_err(model: &str) -> IpcError {
    IpcError::new(
        "model_not_found",
        format!("model {model:?} not installed in Ollama"),
    )
}

/// Truncate `body` to at most `cap` characters (UTF-8 boundary safe).
pub fn excerpt(body: &str, cap: usize) -> String {
    if body.len() <= cap {
        return body.to_owned();
    }
    let mut end = cap;
    while !body.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    body[..end].to_owned()
}

/// Convenience: pull `field` out of a payload map as a non-empty string.
/// Returns `invalid_request` if missing/empty/wrong type.
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
    fn excerpt_respects_char_boundary() {
        let s = "abcdé";
        // The é is 2 bytes; cap=4 lands inside it. Excerpt should back up.
        let out = excerpt(s, 4);
        assert!(out.is_char_boundary(out.len()));
        assert_eq!(out, "abcd");
    }

    #[test]
    fn require_string_rejects_missing_and_blank() {
        let p = json!({});
        assert!(require_string(&p, "model").is_err());
        let p = json!({"model": "  "});
        assert!(require_string(&p, "model").is_err());
        let p = json!({"model": "qwen"});
        assert_eq!(require_string(&p, "model").unwrap(), "qwen");
    }

    #[test]
    fn ollama_http_err_carries_details() {
        let e = ollama_http_err(503, "service unavailable".into());
        assert_eq!(e.code, "ollama_http");
        let det = e.details.as_ref().unwrap();
        assert_eq!(det["status"], 503);
        assert_eq!(det["body_excerpt"], "service unavailable");
    }
}
