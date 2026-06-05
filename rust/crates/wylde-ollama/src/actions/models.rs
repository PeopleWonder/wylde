//! Read-side + lightweight write actions:
//!   * `ollama.health`        — wrapper liveness + upstream GET /api/tags probe
//!   * `ollama.list_models`   — GET /api/tags
//!   * `ollama.list_loaded`   — GET /api/ps
//!   * `ollama.show`          — POST /api/show {model}
//!   * `ollama.delete`        — DELETE /api/delete {name}
//!   * `ollama.eject`         — POST /api/generate {model, keep_alive=0}
//!
//! Each handler accepts an optional upstream override so tests can
//! point at a wiremock server. The production registration in
//! `service.rs` uses the process-wide upstream.

use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::actions::error::{
    excerpt, invalid_request, model_not_found_err, ollama_http_err, ollama_unreachable_err,
    require_string,
};
use crate::config::Config;
use crate::upstream::Upstream;

const BODY_EXCERPT_CAP: usize = 300;

/// `ollama.health` — wrapper liveness **plus** an upstream probe.
///
/// The wrapper is up by definition if this handler runs, so `ok`/`pong`
/// are always `true`. The reply additionally carries the upstream Ollama
/// status (`"ok"` | `"unreachable"` | `"timeout"`) and, when reachable,
/// the installed-model count, so the Dashboard can distinguish "wrapper
/// answering its pipe" from "the LLM layer is actually reachable" — the
/// green-but-unreachable gap. Upstream being down does **not** fail the
/// probe: the wrapper is still reported up, with the upstream flagged.
pub async fn handle_health(_payload: Value, up: Arc<Upstream>) -> Reply {
    let cfg = Config::get();
    let probe = up.probe(cfg.health_timeout_s).await;
    let mut data = json!({
        "ok": true,
        "pong": true,
        "upstream": probe.status.as_str(),
    });
    if let Some(n) = probe.models {
        data["upstream_models"] = json!(n);
    }
    Reply::ok(data)
}

pub async fn handle_list_models(_payload: Value, up: Arc<Upstream>) -> Reply {
    let cfg = Config::get();
    let resp = match up
        .request(Method::GET, "/api/tags", None, cfg.list_models_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    parse_passthrough_json(resp).await
}

pub async fn handle_list_loaded(_payload: Value, up: Arc<Upstream>) -> Reply {
    let cfg = Config::get();
    let resp = match up
        .request(Method::GET, "/api/ps", None, cfg.list_loaded_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    parse_passthrough_json(resp).await
}

pub async fn handle_show(payload: Value, up: Arc<Upstream>) -> Reply {
    let model = match require_string(&payload, "model") {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let cfg = Config::get();
    let body = json!({"model": model});
    let resp = match up
        .request(Method::POST, "/api/show", Some(&body), cfg.show_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    if resp.status() == StatusCode::NOT_FOUND {
        return Reply::err(model_not_found_err(&model));
    }
    parse_passthrough_json(resp).await
}

pub async fn handle_delete(payload: Value, up: Arc<Upstream>) -> Reply {
    // Python sends `{"name": ...}` (ollama_client.py:170); the Ollama
    // /api/delete spec also accepts {"model": ...}. We accept either on
    // the pipe surface and forward `{"name": ...}` upstream to match
    // what Python sends today.
    let name = match payload
        .get("model")
        .or_else(|| payload.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    {
        Some(n) => n,
        None => {
            return Reply::err(invalid_request(
                "payload requires either 'model' or 'name' (string)",
            ));
        }
    };
    let cfg = Config::get();
    let body = json!({"name": name});
    let resp = match up
        .request(
            Method::DELETE,
            "/api/delete",
            Some(&body),
            cfg.delete_timeout_s,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    if resp.status() == StatusCode::NOT_FOUND {
        return Reply::err(model_not_found_err(&name));
    }
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Reply::err(ollama_http_err(status, excerpt(&body, BODY_EXCERPT_CAP)));
    }
    Reply::ok(json!({"ok": true, "freed": true}))
}

pub async fn handle_eject(payload: Value, up: Arc<Upstream>) -> Reply {
    let model = match require_string(&payload, "model") {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let cfg = Config::get();
    // Documented eviction trick: empty-prompt /api/generate with
    // keep_alive=0 tells Ollama to release the model immediately.
    let body = json!({"model": model, "keep_alive": 0});
    let resp = match up
        .request(
            Method::POST,
            "/api/generate",
            Some(&body),
            cfg.eject_timeout_s,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    if resp.status() == StatusCode::NOT_FOUND {
        return Reply::err(model_not_found_err(&model));
    }
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Reply::err(ollama_http_err(status, excerpt(&body, BODY_EXCERPT_CAP)));
    }
    Reply::ok(json!({"ok": true}))
}

/// POST /api/generate with empty prompt + non-zero keep_alive — Ollama
/// loads the model into VRAM and returns without generating tokens.
/// Caller may pass `keep_alive` (string like "24h" or integer seconds);
/// default mirrors Python's `DEFAULT_KEEP_ALIVE = "24h"`.
pub async fn handle_preload(payload: Value, up: Arc<Upstream>) -> Reply {
    let model = match require_string(&payload, "model") {
        Ok(m) => m,
        Err(e) => return Reply::err(e),
    };
    let keep_alive = payload
        .get("keep_alive")
        .cloned()
        .unwrap_or_else(|| Value::String("24h".to_owned()));
    let cfg = Config::get();
    let body = json!({
        "model": model,
        "prompt": "",
        "stream": false,
        "keep_alive": keep_alive,
    });
    // Preload may take a while on cold start (model file → VRAM). Use the
    // chat timeout — same scale as a chat call's first-token latency.
    let resp = match up
        .request(
            Method::POST,
            "/api/generate",
            Some(&body),
            cfg.chat_timeout_s,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    if resp.status() == StatusCode::NOT_FOUND {
        return Reply::err(model_not_found_err(&model));
    }
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Reply::err(ollama_http_err(status, excerpt(&body, BODY_EXCERPT_CAP)));
    }
    Reply::ok(json!({
        "ok": true,
        "model": model,
        "keep_alive": keep_alive,
    }))
}

/// Decode an Ollama JSON response as a passthrough — non-2xx becomes
/// `ollama_http`, non-JSON body becomes `ollama_http` with the raw
/// excerpt. The full Ollama envelope is returned verbatim on success.
async fn parse_passthrough_json(resp: reqwest::Response) -> Reply {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Reply::err(ollama_http_err(
            status.as_u16(),
            excerpt(&body, BODY_EXCERPT_CAP),
        ));
    }
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(ollama_http_err(
            status.as_u16(),
            format!("decode failed: {e}"),
        )),
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
    async fn health_upstream_ok_reports_models() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "qwen2.5:0.5b"}, {"name": "nomic-embed"}]
            })))
            .mount(&server)
            .await;
        let reply = handle_health(Value::Null, up).await;
        assert!(reply.ok);
        assert_eq!(reply.data["ok"], true);
        assert_eq!(reply.data["pong"], true);
        assert_eq!(reply.data["upstream"], "ok");
        assert_eq!(reply.data["upstream_models"], 2);
    }

    #[tokio::test]
    async fn health_upstream_unreachable_still_reports_wrapper_up() {
        // Bind-then-drop a port so it reliably refuses. The wrapper itself
        // is up (this handler ran), so we report ok:true with the upstream
        // flagged unreachable rather than failing the probe.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let up = crate::upstream::for_test(&format!("http://127.0.0.1:{port}"));
        let reply = handle_health(Value::Null, up).await;
        assert!(reply.ok);
        assert_eq!(reply.data["ok"], true);
        assert_eq!(reply.data["pong"], true);
        // A dead loopback port reads as unreachable or (if Windows drops
        // the SYN to the deadline) timeout — either way a down-state, not "ok".
        let upstream = reply.data["upstream"].as_str().unwrap();
        assert!(
            upstream == "unreachable" || upstream == "timeout",
            "expected a down-state, got {upstream:?}"
        );
        assert!(reply.data.get("upstream_models").is_none());
    }

    #[tokio::test]
    async fn health_upstream_timeout_is_flagged() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(1500))
                    .set_body_json(json!({"models": []})),
            )
            .mount(&server)
            .await;
        // health_timeout_s defaults to 3s — too long for a fast test, so
        // probe directly with a 1s deadline against the slow mock.
        let probe = up.probe(1).await;
        assert_eq!(
            probe.status,
            crate::upstream::UpstreamStatus::Timeout
        );
    }

    #[tokio::test]
    async fn list_models_passthrough() {
        let (server, up) = fake_upstream().await;
        let envelope = json!({
            "models": [
                {"name": "qwen2.5:0.5b", "modified_at": "2026-01-01T00:00:00Z",
                 "size": 397807296, "digest": "abc",
                 "details": {"format": "gguf"}}
            ]
        });
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope.clone()))
            .mount(&server)
            .await;
        let reply = handle_list_models(Value::Null, up).await;
        assert!(reply.ok);
        assert_eq!(reply.data, envelope);
    }

    #[tokio::test]
    async fn list_models_passthrough_5xx_is_ollama_http() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream busy"))
            .mount(&server)
            .await;
        let reply = handle_list_models(Value::Null, up).await;
        assert!(!reply.ok);
        let err = reply.error.unwrap();
        assert_eq!(err.code, "ollama_http");
        assert_eq!(err.details.as_ref().unwrap()["status"], 503);
    }

    #[tokio::test]
    async fn list_loaded_passthrough() {
        let (server, up) = fake_upstream().await;
        let envelope = json!({
            "models": [
                {"name": "qwen", "size": 1234, "size_vram": 999,
                 "expires_at": "2026-05-23T12:00:00Z"}
            ]
        });
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope.clone()))
            .mount(&server)
            .await;
        let reply = handle_list_loaded(Value::Null, up).await;
        assert!(reply.ok);
        assert_eq!(reply.data, envelope);
    }

    #[tokio::test]
    async fn show_passthrough_and_404() {
        let (server, up) = fake_upstream().await;
        let envelope = json!({"details": {"family": "qwen"}, "model_info": {}});
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_json(json!({"model": "qwen2.5:0.5b"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_json(json!({"model": "missing"})))
            .respond_with(ResponseTemplate::new(404).set_body_string("model not found"))
            .mount(&server)
            .await;

        let ok = handle_show(json!({"model": "qwen2.5:0.5b"}), up.clone()).await;
        assert!(ok.ok);
        assert_eq!(ok.data, envelope);

        let miss = handle_show(json!({"model": "missing"}), up).await;
        assert!(!miss.ok);
        assert_eq!(miss.error.unwrap().code, "model_not_found");
    }

    #[tokio::test]
    async fn show_requires_model() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let r = handle_show(json!({}), up).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn delete_happy_path() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("DELETE"))
            .and(path("/api/delete"))
            .and(body_json(json!({"name": "qwen"})))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let r = handle_delete(json!({"name": "qwen"}), up).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
        assert_eq!(r.data["freed"], true);
    }

    #[tokio::test]
    async fn delete_accepts_model_alias() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("DELETE"))
            .and(path("/api/delete"))
            .and(body_json(json!({"name": "qwen"})))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let r = handle_delete(json!({"model": "qwen"}), up).await;
        assert!(r.ok);
    }

    #[tokio::test]
    async fn delete_404_is_model_not_found() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("DELETE"))
            .and(path("/api/delete"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let r = handle_delete(json!({"name": "ghost"}), up).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "model_not_found");
    }

    #[tokio::test]
    async fn eject_uses_generate_keep_alive_zero() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(json!({"model": "qwen", "keep_alive": 0})))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let r = handle_eject(json!({"model": "qwen"}), up).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
    }

    #[tokio::test]
    async fn preload_uses_generate_empty_prompt_default_keep_alive() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(json!({
                "model": "qwen",
                "prompt": "",
                "stream": false,
                "keep_alive": "24h",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let r = handle_preload(json!({"model": "qwen"}), up).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
        assert_eq!(r.data["model"], "qwen");
        assert_eq!(r.data["keep_alive"], "24h");
    }

    #[tokio::test]
    async fn preload_passes_caller_keep_alive_through() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(json!({
                "model": "qwen",
                "prompt": "",
                "stream": false,
                "keep_alive": 3600,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let r = handle_preload(json!({"model": "qwen", "keep_alive": 3600}), up).await;
        assert!(r.ok);
        assert_eq!(r.data["keep_alive"], 3600);
    }

    #[tokio::test]
    async fn preload_requires_model() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let r = handle_preload(json!({}), up).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn preload_404_is_model_not_found() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let r = handle_preload(json!({"model": "ghost"}), up).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "model_not_found");
    }
}
