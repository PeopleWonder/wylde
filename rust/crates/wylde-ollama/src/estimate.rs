//! Pre-flight VRAM footprint estimation for a model (design doc §3 step 2).
//!
//! `ollama.chat` / `ollama.chat_stream` / `ollama.embed` must tell the VRAM
//! broker how many bytes to reserve. The broker's *own* estimator only exists
//! in the Rust impl (Phase 0.5); the Python impl — still the deployed default
//! — rejects a missing/zero `bytes` outright with
//! `invalid_request: bytes must be positive`. So the client computes the
//! footprint itself, which is what design doc §3 step 2 mandated all along,
//! and the value then works against *either* broker impl.
//!
//! Preference order (first hit wins):
//!   1. `/api/ps` — when the model is already resident its reported `size`
//!      (total footprint, VRAM + any CPU offload) is the exact figure. The
//!      broker splits that total across VRAM/DRAM itself, so the total is the
//!      right number to hand it.
//!   2. `/api/tags` — on-disk size × `vram_estimate_multiplier` for a cold
//!      model. Conservative headroom for KV-cache / activations (master plan
//!      Q3's "model_size_on_disk * 1.2").
//!
//! When `/api/tags` answers 200 but the model is absent it is genuinely not
//! pulled — surfaced as [`VramEstimate::NotPulled`] so the caller returns an
//! actionable `model_not_found` instead of a cryptic broker rejection. Any
//! transport/endpoint failure degrades to [`VramEstimate::Bytes`] with a
//! conservative default: a flaky `/api/tags` must never block a chat, and it
//! must never let a non-positive `bytes` reach the broker.

use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::config::Config;
use crate::upstream::Upstream;

/// Conservative footprint used when the model size can't be determined but
/// the model is (or may be) present — keeps `bytes` strictly positive so the
/// broker never rejects on `bytes <= 0`. 4 GiB matches the broker's own
/// `estimate_default_vram` default.
const DEFAULT_ESTIMATE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Outcome of [`estimate_vram_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VramEstimate {
    /// A strictly-positive byte estimate to pass as `vram.reserve {bytes}`.
    Bytes(u64),
    /// `/api/tags` answered but the model is not installed in Ollama.
    NotPulled,
}

/// Estimate the VRAM footprint for `model`. See module docs for the order.
pub async fn estimate_vram_bytes(up: &Upstream, model: &str) -> VramEstimate {
    if let Some(bytes) = loaded_footprint(up, model).await {
        return VramEstimate::Bytes(bytes.max(1));
    }
    match on_disk_size(up, model).await {
        TagsLookup::Size(on_disk) => {
            let mult = Config::get().vram_estimate_multiplier;
            let est = (on_disk as f64 * mult) as u64;
            VramEstimate::Bytes(est.max(1))
        }
        TagsLookup::NotFound => VramEstimate::NotPulled,
        TagsLookup::Unavailable => VramEstimate::Bytes(DEFAULT_ESTIMATE_BYTES),
    }
}

/// `/api/ps` total footprint for a resident model, if it is currently loaded.
async fn loaded_footprint(up: &Upstream, model: &str) -> Option<u64> {
    let cfg = Config::get();
    let resp = up
        .request(Method::GET, "/api/ps", None, cfg.list_loaded_timeout_s)
        .await
        .ok()?;
    if resp.status() != StatusCode::OK {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let models = body.get("models")?.as_array()?;
    let entry = models.iter().find(|m| model_matches(m, model))?;
    entry.get("size").and_then(Value::as_u64).filter(|&n| n > 0)
}

/// Result of the `/api/tags` lookup — distinguishes "tags listed but model
/// absent" (genuinely not pulled) from "tags endpoint unreachable / errored"
/// (caller must not treat as not-pulled).
enum TagsLookup {
    Size(u64),
    NotFound,
    Unavailable,
}

async fn on_disk_size(up: &Upstream, model: &str) -> TagsLookup {
    let cfg = Config::get();
    let resp = match up
        .request(Method::GET, "/api/tags", None, cfg.list_models_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(_) => return TagsLookup::Unavailable,
    };
    if resp.status() != StatusCode::OK {
        return TagsLookup::Unavailable;
    }
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return TagsLookup::Unavailable,
    };
    let Some(models) = body.get("models").and_then(Value::as_array) else {
        return TagsLookup::Unavailable;
    };
    match models.iter().find(|m| model_matches(m, model)) {
        Some(m) => match m.get("size").and_then(Value::as_u64) {
            Some(n) if n > 0 => TagsLookup::Size(n),
            // Listed but no usable size — present-but-unknown, not absent.
            _ => TagsLookup::Unavailable,
        },
        None => TagsLookup::NotFound,
    }
}

/// Match an Ollama model-list entry against `want` by its `name` (or `model`
/// alias) field, case-insensitively. The `:tag` suffix is kept verbatim so
/// `:7b` and `:14b` never collide.
fn model_matches(entry: &Value, want: &str) -> bool {
    let want = want.trim();
    ["name", "model"].iter().any(|k| {
        entry
            .get(*k)
            .and_then(Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(want))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GIB: u64 = 1024 * 1024 * 1024;

    async fn fake() -> (MockServer, std::sync::Arc<Upstream>) {
        let server = MockServer::start().await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    #[tokio::test]
    async fn loaded_model_uses_ps_total_size() {
        let (server, up) = fake().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "qwen:7b", "size": 6 * GIB, "size_vram": 5 * GIB}]
            })))
            .mount(&server)
            .await;
        let e = estimate_vram_bytes(&up, "qwen:7b").await;
        assert_eq!(e, VramEstimate::Bytes(6 * GIB));
    }

    #[tokio::test]
    async fn cold_model_uses_tags_size_times_multiplier() {
        let (server, up) = fake().await;
        // /api/ps has no entry for it → fall through to /api/tags.
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "hf.co/u/Big-27B-GGUF:Q4_K_M", "size": 10 * GIB}]
            })))
            .mount(&server)
            .await;
        let e = estimate_vram_bytes(&up, "hf.co/u/Big-27B-GGUF:Q4_K_M").await;
        // 10 GiB × 1.2 (default multiplier) = 12 GiB.
        assert_eq!(e, VramEstimate::Bytes(12 * GIB));
    }

    #[tokio::test]
    async fn absent_from_tags_is_not_pulled() {
        let (server, up) = fake().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "something-else:latest", "size": GIB}]
            })))
            .mount(&server)
            .await;
        let e = estimate_vram_bytes(&up, "ghost:7b").await;
        assert_eq!(e, VramEstimate::NotPulled);
    }

    #[tokio::test]
    async fn tags_endpoint_error_degrades_to_default_not_not_pulled() {
        // No mocks → MockServer 404s every path → tags Unavailable. Must NOT
        // be reported as NotPulled (we can't prove absence) and must still be
        // a strictly-positive estimate so the broker accepts it.
        let (_server, up) = fake().await;
        let e = estimate_vram_bytes(&up, "qwen:7b").await;
        assert_eq!(e, VramEstimate::Bytes(DEFAULT_ESTIMATE_BYTES));
    }
}
