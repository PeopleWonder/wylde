//! OpenAI-shaped errors: `{"error": {"message", "type", "param", "code"}}`.
//!
//! OpenAI SDKs parse this shape (not Wylde's `{ok:false,error}` envelope),
//! so every `/v1` failure goes through [`OpenAiError`]. Internal details
//! never leak: a pipe or upstream failure becomes a generic message, and
//! only caller-facing facts (the unknown model id, the bad field) are
//! echoed back.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::auth::AuthError;

/// One `/v1` error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiError {
    pub status: StatusCode,
    pub message: String,
    /// OpenAI's `type`: `invalid_request_error`, `authentication_error`,
    /// `rate_limit_error` or `server_error`.
    pub kind: &'static str,
    pub param: Option<String>,
    pub code: Option<&'static str>,
    /// Seconds for a `Retry-After` header (429 / 503).
    pub retry_after: Option<u64>,
}

impl OpenAiError {
    fn new(status: StatusCode, kind: &'static str, code: &'static str, message: String) -> Self {
        Self {
            status,
            message,
            kind,
            param: None,
            code: Some(code),
            retry_after: None,
        }
    }

    /// 400 — a malformed request; `param` names the offending field.
    pub fn invalid_request(message: impl Into<String>, param: Option<&str>) -> Self {
        let mut e = Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request_error",
            message.into(),
        );
        e.param = param.map(str::to_owned);
        e
    }

    /// 401 — missing or unrecognised device token.
    pub fn invalid_api_key(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            message.into(),
        )
    }

    /// 404 — the model id (or alias) isn't known.
    pub fn model_not_found(model: &str) -> Self {
        let mut e = Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("The model '{model}' does not exist"),
        );
        e.param = Some("model".to_owned());
        e
    }

    /// 429 — over the `/v1` rate limit.
    pub fn rate_limited(limit: u32, retry_after: u64) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "rate_limit_exceeded",
            format!("Rate limit reached: {limit} requests per minute"),
        );
        e.retry_after = Some(retry_after);
        e
    }

    /// 503 — no VRAM for the model right now.
    pub fn insufficient_vram() -> Self {
        let mut e = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "insufficient_vram",
            "Not enough GPU memory to serve this model right now".to_owned(),
        );
        e.retry_after = Some(5);
        e
    }

    /// 502 — Ollama or a Wylde service couldn't be reached.
    pub fn upstream_unavailable() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "upstream_unavailable",
            "The inference backend is unavailable".to_owned(),
        )
    }

    /// 503 — the device-gate couldn't verify the token right now.
    pub fn auth_unavailable() -> Self {
        let mut e = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "auth_unavailable",
            "The device-gate is unavailable; retry shortly".to_owned(),
        );
        e.retry_after = Some(5);
        e
    }

    /// 500 — anything unexpected (details stay in the log).
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "internal_error",
            "The server had an error processing the request".to_owned(),
        )
    }

    /// Map a device-token rejection. Missing and invalid tokens are both
    /// `invalid_api_key` (401); an unreachable device-gate is a 503 so the
    /// client retries instead of discarding a still-valid key.
    pub fn from_auth(err: &AuthError) -> Self {
        match err {
            AuthError::MissingToken => Self::invalid_api_key(
                "Missing API key: pass a Wylde device token as 'Authorization: Bearer <token>'",
            ),
            AuthError::InvalidToken(_) => Self::invalid_api_key("Incorrect API key provided"),
            AuthError::Unavailable(_) => Self::auth_unavailable(),
        }
    }

    /// Map a `wylde-ollama` / harness pipe error. `model` is the id the
    /// caller asked for (echoed in a 404).
    pub fn from_ipc(err: &IpcError, model: &str) -> Self {
        match err.code.as_str() {
            "model_not_found" => Self::model_not_found(model),
            "invalid_request" => Self::invalid_request(err.message.clone(), None),
            "vram_admission_denied" | "insufficient_vram" | "would_exceed_total" => {
                Self::insufficient_vram()
            }
            "ollama_unreachable" | "ollama_http" | "broker_unreachable" | "pipe_unavailable"
            | "pipe_connect" | "pipe_timeout" | "pipe_io" | "handshake_timeout"
            | "handshake_io" | "handshake_rejected" => Self::upstream_unavailable(),
            other => {
                tracing::warn!("openai: unmapped pipe error {other}: {}", err.message);
                Self::internal()
            }
        }
    }

    /// The JSON body.
    pub fn body(&self) -> Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": self.param,
                "code": self.code,
            }
        })
    }
}

impl IntoResponse for OpenAiError {
    fn into_response(self) -> Response {
        let mut resp = (self.status, Json(self.body())).into_response();
        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_the_openai_shape() {
        let b = OpenAiError::model_not_found("ghost").body();
        assert_eq!(b["error"]["type"], "invalid_request_error");
        assert_eq!(b["error"]["code"], "model_not_found");
        assert_eq!(b["error"]["param"], "model");
        assert!(b["error"]["message"].as_str().unwrap().contains("ghost"));
        assert!(b.get("ok").is_none(), "no Wylde envelope");
    }

    #[test]
    fn rate_limited_sets_retry_after() {
        let r = OpenAiError::rate_limited(600, 17).into_response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()[header::RETRY_AFTER], "17");
    }

    #[test]
    fn ipc_errors_map_to_status_without_leaking() {
        let cases = [
            ("model_not_found", StatusCode::NOT_FOUND),
            ("invalid_request", StatusCode::BAD_REQUEST),
            ("vram_admission_denied", StatusCode::SERVICE_UNAVAILABLE),
            ("insufficient_vram", StatusCode::SERVICE_UNAVAILABLE),
            ("ollama_unreachable", StatusCode::BAD_GATEWAY),
            ("pipe_unavailable", StatusCode::BAD_GATEWAY),
            ("something_new", StatusCode::INTERNAL_SERVER_ERROR),
        ];
        for (code, status) in cases {
            let e = OpenAiError::from_ipc(&IpcError::new(code, "secret internal path C:/x"), "m");
            assert_eq!(e.status, status, "{code}");
            if status.is_server_error() {
                assert!(
                    !e.message.contains("secret"),
                    "{code} leaked: {}",
                    e.message
                );
            }
        }
    }

    #[test]
    fn auth_errors_map_to_401_or_503() {
        assert_eq!(
            OpenAiError::from_auth(&AuthError::MissingToken).status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            OpenAiError::from_auth(&AuthError::InvalidToken("x")).code,
            Some("invalid_api_key")
        );
        assert_eq!(
            OpenAiError::from_auth(&AuthError::Unavailable(502)).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
