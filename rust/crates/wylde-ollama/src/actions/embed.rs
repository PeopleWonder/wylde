//! `ollama.embed` — POST /api/embed.
//!
//! Per design doc §3 and the Wylde user's 2026-05-22 decision, embed *does* go
//! through the broker. The broker's dedupe-by-nonce fast path makes
//! repeated leases for the same (service, model) effectively free. The
//! lease is dropped right after the response lands.
//!
//! An escape hatch is honoured: `WYLDE_OLLAMA_EMBED_SKIP_BROKER=1`
//! routes embed straight to upstream with no lease — for debugging when
//! the broker is temporarily down and embeddings shouldn't block on it.

use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::actions::error::{
    excerpt, invalid_request, model_not_found_err, ollama_http_err, ollama_unreachable_err,
    require_string,
};
use crate::config::Config;
use crate::estimate::{estimate_vram_bytes, VramEstimate};
use crate::lease::{self, LeaseRequest, Priority};
use crate::upstream::Upstream;

const BODY_EXCERPT_CAP: usize = 300;

pub async fn handle_embed(payload: Value, up: Arc<Upstream>) -> Reply {
    let model = match require_string(&payload, "model") {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };

    // `input` is required and must be a non-empty array of strings or
    // a single string. Pass through whatever shape the caller sent —
    // the upstream Ollama /api/embed accepts both shapes.
    let input = payload.get("input").cloned();
    let input = match input {
        Some(v) if v.is_string() || v.is_array() => v,
        _ => {
            return Reply::err(invalid_request(
                "payload.input is required (string or array of strings)",
            ));
        }
    };

    let cfg = Config::get();
    let lease_guard = if cfg.embed_skip_broker {
        None
    } else {
        // Compute the footprint (design §3 step 2) so the broker gets a
        // positive `bytes` — embed models are small but the Python broker
        // still rejects a missing one. An absent model is surfaced as an
        // actionable `model_not_found` before any reserve or upstream call.
        let bytes_hint = match estimate_vram_bytes(&up, &model).await {
            VramEstimate::Bytes(b) => Some(b),
            VramEstimate::NotPulled => return Reply::err(model_not_found_err(&model)),
        };
        // Best-effort lease. If the broker is unreachable, log and
        // continue rather than blocking embeddings on broker health.
        // The harness already tolerates embed errors so we lean toward
        // availability over strict bookkeeping here.
        match lease::acquire(LeaseRequest {
            model: model.clone(),
            bytes_hint,
            priority: Priority::Default,
            nonce: None,
        })
        .await
        {
            Ok(l) => Some(l),
            Err(e) => {
                if e.code == "vram_admission_denied" {
                    // Real admission denial — surface it so the caller
                    // knows the GPU is full.
                    return Reply::err(e);
                }
                tracing::warn!(
                    "wylde-ollama: embed lease unavailable ({}), proceeding without: {}",
                    e.code,
                    e.message,
                );
                None
            }
        }
    };

    let body = json!({"model": model, "input": input});
    let resp = match up
        .request(Method::POST, "/api/embed", Some(&body), cfg.embed_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };

    if resp.status() == StatusCode::NOT_FOUND {
        // Drop the lease before returning.
        if let Some(l) = lease_guard {
            l.release().await;
        }
        return Reply::err(model_not_found_err(&model));
    }
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if let Some(l) = lease_guard {
            l.release().await;
        }
        return Reply::err(ollama_http_err(status, excerpt(&body, BODY_EXCERPT_CAP)));
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            if let Some(l) = lease_guard {
                l.release().await;
            }
            return Reply::err(ollama_unreachable_err(&e));
        }
    };

    if let Some(l) = lease_guard {
        l.release().await;
    }

    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(ollama_http_err(200, format!("decode failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn fake_upstream() -> (MockServer, Arc<Upstream>) {
        let server = MockServer::start().await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    #[tokio::test]
    async fn embed_happy_path_no_broker() {
        // Skip broker to keep this test pure-upstream.
        std::env::set_var("WYLDE_OLLAMA_EMBED_SKIP_BROKER", "1");
        let (server, up) = fake_upstream().await;
        let envelope = json!({
            "embeddings": [[0.1, 0.2, 0.3]]
        });
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .and(body_json(json!({"model": "nomic-embed-text", "input": ["hello"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope.clone()))
            .mount(&server)
            .await;
        let r = handle_embed(
            json!({"model": "nomic-embed-text", "input": ["hello"]}),
            up,
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.data, envelope);
        std::env::remove_var("WYLDE_OLLAMA_EMBED_SKIP_BROKER");
    }

    #[tokio::test]
    async fn embed_404_is_model_not_found() {
        std::env::set_var("WYLDE_OLLAMA_EMBED_SKIP_BROKER", "1");
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .respond_with(ResponseTemplate::new(404).set_body_string("model missing"))
            .mount(&server)
            .await;
        let r = handle_embed(
            json!({"model": "ghost-embed", "input": "hello"}),
            up,
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "model_not_found");
        std::env::remove_var("WYLDE_OLLAMA_EMBED_SKIP_BROKER");
    }

    #[tokio::test]
    async fn embed_requires_model_and_input() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let r = handle_embed(json!({"input": ["x"]}), up.clone()).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");

        let r = handle_embed(json!({"model": "x"}), up).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }
}
