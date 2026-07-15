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

use std::borrow::Cow;

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
/// alias) field, case-insensitively.
///
/// A bare name carries an implicit `:latest` tag in Ollama, so we normalise the
/// implicit tag on *both* sides before comparing: a request for
/// `nomic-embed-text` matches a listed `nomic-embed-text:latest` (and vice
/// versa). Explicit sized tags are kept verbatim, so `:7b` and `:14b` never
/// collide and neither is mistaken for `:latest`.
fn model_matches(entry: &Value, want: &str) -> bool {
    let want = with_implicit_latest(want.trim());
    ["name", "model"].iter().any(|k| {
        entry
            .get(*k)
            .and_then(Value::as_str)
            .is_some_and(|n| with_implicit_latest(n.trim()).eq_ignore_ascii_case(&want))
    })
}

/// Append the implicit `:latest` tag to a bare model name. The tag separator is
/// the last `:` that follows the final `/` (so registry/namespace paths like
/// `hf.co/u/Repo-GGUF:Q4` are handled, and a host `localhost:11434/...` colon is
/// not mistaken for a tag). Already-tagged names are returned unchanged.
fn with_implicit_latest(name: &str) -> Cow<'_, str> {
    let last_segment = name.rsplit('/').next().unwrap_or(name);
    if last_segment.contains(':') {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("{name}:latest"))
    }
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

    // --- F3: bare name ⇄ :latest tag resolution (model_matches) ---

    #[test]
    fn bare_name_matches_latest_entry() {
        // The live bug: requesting `nomic-embed-text` must match the installed
        // `nomic-embed-text:latest` entry from /api/tags.
        let entry = json!({"name": "nomic-embed-text:latest"});
        assert!(model_matches(&entry, "nomic-embed-text"));
    }

    #[test]
    fn latest_request_matches_bare_entry() {
        // Symmetric: an explicit :latest request matches a bare listed name.
        let entry = json!({"name": "nomic-embed-text"});
        assert!(model_matches(&entry, "nomic-embed-text:latest"));
    }

    #[test]
    fn bare_matches_bare_and_latest_matches_latest() {
        assert!(model_matches(
            &json!({"name": "nomic-embed-text"}),
            "nomic-embed-text"
        ));
        assert!(model_matches(
            &json!({"name": "nomic-embed-text:latest"}),
            "nomic-embed-text:latest"
        ));
    }

    #[test]
    fn sized_tags_never_collide() {
        // :7b and :14b are distinct and neither resolves to :latest.
        let seven = json!({"name": "qwen:7b"});
        assert!(model_matches(&seven, "qwen:7b"));
        assert!(!model_matches(&seven, "qwen:14b"));
        assert!(!model_matches(&seven, "qwen")); // bare → qwen:latest ≠ qwen:7b
        assert!(!model_matches(&seven, "qwen:latest"));
    }

    #[test]
    fn registry_path_colon_is_not_a_host_tag() {
        // A namespaced GGUF tag still matches itself; the bare form gets :latest.
        let entry = json!({"name": "hf.co/u/Repo-GGUF:Q4_K_M"});
        assert!(model_matches(&entry, "hf.co/u/Repo-GGUF:Q4_K_M"));
        assert!(!model_matches(&entry, "hf.co/u/Repo-GGUF")); // → :latest ≠ :Q4_K_M
    }

    #[tokio::test]
    async fn cold_bare_name_resolves_latest_size_from_tags() {
        // End-to-end through estimate: bare request, :latest entry on disk.
        let (server, up) = fake().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "nomic-embed-text:latest", "size": GIB}]
            })))
            .mount(&server)
            .await;
        let e = estimate_vram_bytes(&up, "nomic-embed-text").await;
        // Present (not NotPulled): bare name resolved to the :latest entry.
        assert!(matches!(e, VramEstimate::Bytes(_)), "got {e:?}");
    }
}
