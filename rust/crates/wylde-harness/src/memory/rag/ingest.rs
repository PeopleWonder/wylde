//! N8N ingest webhook trigger — Rust port of
//! `Core/harness/memory/ingest.py`.
//!
//! Per the Wylde N8N principle, the actual ingestion pipeline (file
//! discovery, chunking, entity extraction, embedding, vector upsert,
//! graph upsert) lives in N8N. The harness only owns the *trigger*:
//! POST a small JSON payload to a webhook URL, return the execution id
//! the workflow assigned (or a structured error envelope on failure).
//!
//! ## Transport (Seam 4 of the 2026-05-28 cleanup slice)
//!
//! [`trigger_ingest`] POSTs the request to `<base_url>/<webhook>` and
//! returns the parsed JSON body on success. Failures map to a stable
//! `{ok: false, error: <code>, detail: <msg>, url: <…>}` envelope so
//! the `rag_index` / `rag_reindex` model-callable tools always see a
//! deterministic shape. Error codes:
//!
//! * `connect_failed`  — TCP/DNS/timeout reaching N8N.
//! * `http_error`      — non-2xx from N8N (status + body excerpt
//!   captured under `detail`).
//! * `decode_failed`   — 2xx body wasn't valid JSON.
//! * `request_failed`  — anything else reqwest surfaces.
//!
//! ## Timeout
//!
//! A 30-second total deadline applies to every call. Set via
//! `WYLDE_N8N_INGEST_TIMEOUT_S`; defaults to 30. Long-running indexing
//! work happens inside N8N — the webhook should return promptly with
//! an execution id.

use std::collections::HashMap;
use std::time::Duration;

use once_cell::sync::Lazy;
use reqwest::{Client, StatusCode};
use serde::Serialize;
use serde_json::{json, Value};

const DEFAULT_TIMEOUT_S: u64 = 30;
const BODY_EXCERPT_CAP: usize = 300;

/// Process-wide reqwest client. Connection pool is shared; the
/// per-call timeout is set on the request builder so it can vary by
/// env var. Mirrors the wylde-ollama / wylde-vram-broker pattern.
static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("wylde-harness/rag-ingest")
        .build()
        .expect("reqwest client construction must not fail")
});

/// Pure-data trigger request. Same fields the Python module accepts.
#[derive(Debug, Clone, Serialize)]
pub struct IngestRequest {
    pub target_path: String,
    pub workspace_id: String,
    pub paths: Option<Vec<String>>,
    pub options: Option<HashMap<String, Value>>,
}

/// Default base URL — mirrors `WYLDE_N8N_BASE_URL`. Stripped trailing
/// slash matches Python.
pub fn n8n_base_url() -> String {
    let raw =
        std::env::var("WYLDE_N8N_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:5678".into());
    raw.trim_end_matches('/').to_owned()
}

/// Default webhook path — mirrors `WYLDE_N8N_INGEST_WEBHOOK`. Leading
/// slash stripped, fallback applied just like Python.
pub fn ingest_webhook() -> String {
    let raw =
        std::env::var("WYLDE_N8N_INGEST_WEBHOOK").unwrap_or_else(|_| "webhook/wylde-ingest".into());
    let stripped = raw.trim_start_matches('/');
    if stripped.is_empty() {
        "webhook/wylde-ingest".into()
    } else {
        stripped.to_owned()
    }
}

/// Compose the full webhook URL the way Python does.
pub fn webhook_url() -> String {
    format!("{}/{}", n8n_base_url(), ingest_webhook())
}

fn timeout_secs() -> u64 {
    std::env::var("WYLDE_N8N_INGEST_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(DEFAULT_TIMEOUT_S)
}

/// Truncate `body` at a char boundary, capped at `cap`. Same shape as
/// `wylde-ollama`'s `excerpt` helper.
fn excerpt(body: &str, cap: usize) -> String {
    if body.len() <= cap {
        return body.to_owned();
    }
    let mut end = cap;
    while !body.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    body[..end].to_owned()
}

/// Trigger the ingest workflow. POSTs the request body to
/// `webhook_url()`; returns the parsed JSON on success or a structured
/// error envelope on failure. Always returns a `Value` — the caller is
/// `run_rag_index` / `run_rag_reindex`, which expect a deterministic
/// `{ok, ...}` shape regardless of transport outcome.
pub async fn trigger_ingest(req: IngestRequest) -> Value {
    let url = webhook_url();
    let timeout = Duration::from_secs(timeout_secs());

    let body = match serde_json::to_value(&req) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("rag::ingest::trigger_ingest: serialize failed: {e}");
            return error_envelope("request_failed", format!("serialize body: {e}"), &url);
        }
    };

    let resp = match HTTP_CLIENT
        .post(&url)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("rag::ingest::trigger_ingest: connect/send failed: {e}");
            let code = if e.is_timeout() || e.is_connect() {
                "connect_failed"
            } else {
                "request_failed"
            };
            return error_envelope(code, e.to_string(), &url);
        }
    };

    let status = resp.status();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return error_envelope("request_failed", format!("read body: {e}"), &url),
    };

    if !status.is_success() {
        let body_excerpt = excerpt(&String::from_utf8_lossy(&bytes), BODY_EXCERPT_CAP);
        return http_error_envelope(status, body_excerpt, &url);
    }

    // 2xx — try to decode. A webhook may legally return an empty body;
    // surface that as `{ok: true}` with no extra fields rather than an
    // error.
    if bytes.is_empty() {
        return json!({"ok": true, "url": url, "status": status.as_u16()});
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut v) => {
            // Stamp `ok: true` if the webhook didn't explicitly set it,
            // so the caller's `ok` branch fires on the typical N8N
            // "200 + {executionId: ...}" reply.
            if v.get("ok").is_none() {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("ok".to_owned(), Value::Bool(true));
                }
            }
            v
        }
        Err(e) => {
            let body_excerpt = excerpt(&String::from_utf8_lossy(&bytes), BODY_EXCERPT_CAP);
            error_envelope(
                "decode_failed",
                format!("decode body: {e}; excerpt: {body_excerpt}"),
                &url,
            )
        }
    }
}

fn error_envelope(code: &str, detail: impl Into<String>, url: &str) -> Value {
    json!({
        "ok": false,
        "error": code,
        "detail": detail.into(),
        "url": url,
    })
}

fn http_error_envelope(status: StatusCode, body_excerpt: String, url: &str) -> Value {
    json!({
        "ok": false,
        "error": "http_error",
        "detail": format!("n8n returned {}: {}", status.as_u16(), body_excerpt),
        "status": status.as_u16(),
        "body_excerpt": body_excerpt,
        "url": url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::rag::test_support::TestEnv;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn req() -> IngestRequest {
        IngestRequest {
            target_path: "/tmp/foo".into(),
            workspace_id: "ws-test".into(),
            paths: Some(vec!["a".into(), "b".into()]),
            options: None,
        }
    }

    fn pin_base_and_path(base: &str, path: &str) {
        std::env::set_var("WYLDE_N8N_BASE_URL", base);
        std::env::set_var("WYLDE_N8N_INGEST_WEBHOOK", path);
    }

    fn clear_env() {
        std::env::remove_var("WYLDE_N8N_BASE_URL");
        std::env::remove_var("WYLDE_N8N_INGEST_WEBHOOK");
        std::env::remove_var("WYLDE_N8N_INGEST_TIMEOUT_S");
    }

    #[test]
    fn webhook_url_uses_env_overrides() {
        let _env = TestEnv::new();
        clear_env();
        std::env::set_var("WYLDE_N8N_BASE_URL", "http://example/");
        std::env::set_var("WYLDE_N8N_INGEST_WEBHOOK", "/custom/path");
        assert_eq!(webhook_url(), "http://example/custom/path");
        clear_env();
    }

    #[test]
    fn webhook_url_falls_back_to_default() {
        let _env = TestEnv::new();
        clear_env();
        let url = webhook_url();
        assert_eq!(url, "http://127.0.0.1:5678/webhook/wylde-ingest");
    }

    #[test]
    fn ingest_webhook_blank_env_falls_back() {
        let _env = TestEnv::new();
        clear_env();
        std::env::set_var("WYLDE_N8N_INGEST_WEBHOOK", "/");
        assert_eq!(ingest_webhook(), "webhook/wylde-ingest");
        clear_env();
    }

    #[test]
    fn timeout_secs_falls_back_to_30() {
        let _env = TestEnv::new();
        std::env::remove_var("WYLDE_N8N_INGEST_TIMEOUT_S");
        assert_eq!(timeout_secs(), 30);
    }

    #[test]
    fn timeout_secs_honours_positive_env_value() {
        let _env = TestEnv::new();
        std::env::set_var("WYLDE_N8N_INGEST_TIMEOUT_S", "5");
        assert_eq!(timeout_secs(), 5);
        std::env::remove_var("WYLDE_N8N_INGEST_TIMEOUT_S");
    }

    #[test]
    fn timeout_secs_clamps_zero_and_garbage_to_default() {
        let _env = TestEnv::new();
        std::env::set_var("WYLDE_N8N_INGEST_TIMEOUT_S", "0");
        assert_eq!(timeout_secs(), 30);
        std::env::set_var("WYLDE_N8N_INGEST_TIMEOUT_S", "not-a-number");
        assert_eq!(timeout_secs(), 30);
        std::env::remove_var("WYLDE_N8N_INGEST_TIMEOUT_S");
    }

    #[test]
    fn excerpt_truncates_at_utf8_boundary() {
        let s = "日本語テスト";
        let trimmed = excerpt(s, 4);
        // 日 is 3 bytes — cap=4 lands inside 本, so the truncation
        // must back up to a char boundary.
        assert!(trimmed.chars().all(|c| c == '日' || c == '本'));
        assert!(s.starts_with(&trimmed));
    }

    #[tokio::test]
    async fn trigger_ingest_returns_decoded_body_on_2xx() {
        let _env = TestEnv::new();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhook/wylde-ingest"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "executionId": "exec_42"
            })))
            .mount(&server)
            .await;
        pin_base_and_path(&server.uri(), "/webhook/wylde-ingest");

        let r = trigger_ingest(req()).await;
        // Webhook didn't explicitly set ok; we stamp ok=true for 2xx.
        assert_eq!(r["ok"], true);
        assert_eq!(r["executionId"], "exec_42");
        clear_env();
    }

    #[tokio::test]
    async fn trigger_ingest_preserves_webhook_ok_when_set() {
        let _env = TestEnv::new();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhook/wylde-ingest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": false,
                "error": "workflow_failed",
            })))
            .mount(&server)
            .await;
        pin_base_and_path(&server.uri(), "/webhook/wylde-ingest");

        let r = trigger_ingest(req()).await;
        // We do NOT overwrite the webhook's explicit ok=false.
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "workflow_failed");
        clear_env();
    }

    #[tokio::test]
    async fn trigger_ingest_handles_empty_2xx_body() {
        let _env = TestEnv::new();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhook/wylde-ingest"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        pin_base_and_path(&server.uri(), "/webhook/wylde-ingest");

        let r = trigger_ingest(req()).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["status"], 204);
        clear_env();
    }

    #[tokio::test]
    async fn trigger_ingest_maps_4xx_to_http_error_envelope() {
        let _env = TestEnv::new();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhook/wylde-ingest"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        pin_base_and_path(&server.uri(), "/webhook/wylde-ingest");

        let r = trigger_ingest(req()).await;
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "http_error");
        assert_eq!(r["status"], 404);
        assert_eq!(r["body_excerpt"], "not found");
        clear_env();
    }

    #[tokio::test]
    async fn trigger_ingest_maps_5xx_to_http_error_envelope() {
        let _env = TestEnv::new();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhook/wylde-ingest"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;
        pin_base_and_path(&server.uri(), "/webhook/wylde-ingest");

        let r = trigger_ingest(req()).await;
        assert_eq!(r["error"], "http_error");
        assert_eq!(r["status"], 503);
        clear_env();
    }

    #[tokio::test]
    async fn trigger_ingest_maps_2xx_with_garbage_body_to_decode_failed() {
        let _env = TestEnv::new();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhook/wylde-ingest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("this is not json")
                    .insert_header("content-type", "text/plain"),
            )
            .mount(&server)
            .await;
        pin_base_and_path(&server.uri(), "/webhook/wylde-ingest");

        let r = trigger_ingest(req()).await;
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "decode_failed");
        clear_env();
    }

    #[tokio::test]
    async fn trigger_ingest_maps_unreachable_to_connect_failed() {
        let _env = TestEnv::new();
        // Port 1 — 127.0.0.1:1 is reserved + unreachable on test machines.
        pin_base_and_path("http://127.0.0.1:1", "/webhook/wylde-ingest");
        std::env::set_var("WYLDE_N8N_INGEST_TIMEOUT_S", "1");

        let r = trigger_ingest(req()).await;
        assert_eq!(r["ok"], false);
        // Either connect_failed or request_failed — both are stable
        // unreachable surfaces. Pin both as acceptable to avoid
        // platform-specific flakiness.
        let err = r["error"].as_str().unwrap_or("");
        assert!(
            err == "connect_failed" || err == "request_failed",
            "expected connect_failed/request_failed, got {err}"
        );
        clear_env();
    }
}
