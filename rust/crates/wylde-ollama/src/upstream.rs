//! Warm `reqwest::Client` for talking to the local Ollama daemon.
//!
//! One `Client` per process, shared via `Arc`. Per design doc §4:
//!   * `pool_idle_timeout(90s)`
//!   * `pool_max_idle_per_host(16)`
//!   * `tcp_keepalive(60s)`
//!   * no per-Client timeout; per-call `.timeout(...)` on the request
//!     builder instead so each action can pick its own deadline.
//!
//! ## Resume resilience
//!
//! After a Windows suspend/resume, established TCP connections to
//! `127.0.0.1:11434` are dead but a pooled `reqwest` connection still
//! looks reusable, so the first post-resume request writes to a dead
//! socket and surfaces as `ollama_unreachable` even though `ollama.exe`
//! is up and healthy. `pool_idle_timeout` does **not** save us here: the
//! monotonic clock hyper uses for idle accounting is frozen across a
//! system suspend, so the stale connection reads as "recently used" and
//! is reused regardless of the idle timeout. Lowering the timeout would
//! not help. Instead every send goes through [`Upstream::request`],
//! which retries **once** on a connection-level error. Such errors mean
//! the upstream never received the request, so the retry is safe even for
//! non-idempotent POSTs, and the second attempt dials a fresh connection
//! after the dead one is evicted from the pool.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Method, Response};
use serde_json::Value;

use crate::config::Config;

#[derive(Debug)]
pub struct Upstream {
    pub client: Client,
    pub base_url: String,
}

/// Upstream liveness as seen by the health probe. Mirrors the strings
/// the `ollama.health` action surfaces (`"ok"`, `"unreachable"`,
/// `"timeout"`) so the Dashboard can flag the LLM layer independently of
/// the wrapper pipe being up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    Ok,
    Unreachable,
    Timeout,
}

impl UpstreamStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
        }
    }
}

/// Result of a cheap upstream health probe: a status plus, when
/// reachable, the installed-model count from `/api/tags` and the
/// round-trip latency in milliseconds. The Dashboard uses `latency_ms`
/// to flag a reachable-but-slow upstream (`status == Ok` but the daemon
/// took >2s) as a degraded/yellow tile, distinct from a healthy green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamProbe {
    pub status: UpstreamStatus,
    pub models: Option<u64>,
    /// Round-trip latency of the `/api/tags` probe. `Some` only when the
    /// upstream actually answered (`status == Ok`); `None` for
    /// unreachable/timeout where there is no meaningful latency.
    pub latency_ms: Option<u64>,
}

/// True for send errors where the request never reached the upstream —
/// a connect failure, or a pooled keep-alive connection severed across a
/// suspend/resume (broken pipe / connection reset / "closed before
/// message completed"). These are safe to retry even for POSTs because
/// Ollama never saw the first attempt. Timeouts and body-read errors are
/// deliberately excluded: the upstream may have already acted on them.
fn is_stale_connection_error(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_body() {
        return false;
    }
    if e.is_connect() {
        return true;
    }
    // reqwest exposes no dedicated "connection reset" predicate, so walk
    // the error's source chain and match the hyper/io text.
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        let msg = s.to_string().to_ascii_lowercase();
        if msg.contains("connection reset")
            || msg.contains("connection closed")
            || msg.contains("connection aborted")
            || msg.contains("broken pipe")
            || msg.contains("closed before message completed")
        {
            return true;
        }
        src = s.source();
    }
    false
}

impl Upstream {
    /// Build a Client with the design-doc pool settings.
    pub fn new(cfg: &Config) -> Result<Self> {
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(cfg.pool_idle_timeout_s))
            .pool_max_idle_per_host(cfg.pool_max_idle_per_host)
            .tcp_keepalive(Duration::from_secs(cfg.tcp_keepalive_s))
            // Intentionally no global timeout — per-call deadlines only.
            .build()
            .context("build reqwest::Client")?;
        Ok(Self {
            client,
            base_url: cfg.ollama_url.clone(),
        })
    }

    /// `GET /` — Ollama returns "Ollama is running" with 200 when healthy.
    /// Used by the boot-time warm probe; routed through the same
    /// retry-once-on-stale-connection path as every other call.
    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/", self.base_url);
        let resp = self
            .request(Method::GET, "/", None, Config::get().health_timeout_s)
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("upstream returned {} for {url}", resp.status());
        }
        Ok(())
    }

    /// Cheap upstream liveness for the `ollama.health` action: `GET
    /// /api/tags` with a short deadline. Never errors — the caller wants
    /// a status, not a `Result`. Distinguishes a timeout from an outright
    /// unreachable upstream and, on success, reports the installed-model
    /// count. Goes through the retry path so a stale post-resume
    /// connection reconnects instead of false-reddening the dashboard.
    pub async fn probe(&self, timeout_s: u64) -> UpstreamProbe {
        let started = std::time::Instant::now();
        match self
            .request(Method::GET, "/api/tags", None, timeout_s)
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                // Measure latency at first-byte (status received) rather
                // than after the body decode below, so a large /api/tags
                // payload doesn't inflate the "is the daemon responsive?"
                // signal the Dashboard reads.
                let latency_ms = started.elapsed().as_millis() as u64;
                let models = resp.json::<Value>().await.ok().and_then(|v| {
                    v.get("models")
                        .and_then(Value::as_array)
                        .map(|a| a.len() as u64)
                });
                UpstreamProbe {
                    status: UpstreamStatus::Ok,
                    models,
                    latency_ms: Some(latency_ms),
                }
            }
            // A non-2xx means something is answering on the port but the
            // model API isn't serving — treat the LLM layer as down.
            Ok(_) => UpstreamProbe {
                status: UpstreamStatus::Unreachable,
                models: None,
                latency_ms: None,
            },
            Err(e) if e.is_timeout() => UpstreamProbe {
                status: UpstreamStatus::Timeout,
                models: None,
                latency_ms: None,
            },
            Err(_) => UpstreamProbe {
                status: UpstreamStatus::Unreachable,
                models: None,
                latency_ms: None,
            },
        }
    }

    /// Issue a JSON request with the given method, path, body, and
    /// per-call timeout. Returns the raw `Response` so callers can pick
    /// JSON vs stream consumption per-action. Retries once on a
    /// connection-level error (see [`is_stale_connection_error`]) so a
    /// suspend-severed pooled connection reconnects transparently.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout_s: u64,
    ) -> reqwest::Result<Response> {
        match self.send_once(method.clone(), path, body, timeout_s).await {
            Ok(resp) => Ok(resp),
            Err(e) if is_stale_connection_error(&e) => {
                tracing::warn!(
                    "wylde-ollama: upstream connection stale (likely suspend/resume), \
                     retrying {method} {path} once on a fresh connection: {e}"
                );
                self.send_once(method, path, body, timeout_s).await
            }
            Err(e) => Err(e),
        }
    }

    /// Single attempt — no retry. [`Upstream::request`] wraps this with
    /// the reconnect-once policy.
    async fn send_once(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout_s: u64,
    ) -> reqwest::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .client
            .request(method, &url)
            .timeout(Duration::from_secs(timeout_s));
        if let Some(b) = body {
            req = req.json(b);
        }
        req.send().await
    }
}

static UPSTREAM: OnceLock<Arc<Upstream>> = OnceLock::new();

/// Process-wide shared upstream client. Built lazily on first access.
pub fn client() -> Arc<Upstream> {
    UPSTREAM
        .get_or_init(|| {
            let cfg = Config::get();
            Arc::new(
                Upstream::new(cfg).expect("upstream reqwest::Client construction failed at boot"),
            )
        })
        .clone()
}

/// Test-only: replace the process-wide upstream with one pointed at the
/// given base URL (e.g. a wiremock server). Subsequent `client()` calls
/// must return the override; the OnceLock pattern doesn't allow replace
/// once set, so this builds a fresh `Upstream` and returns it but does
/// NOT mutate the global. Tests pass the returned Arc directly to action
/// handlers via the `with_upstream(...)` test hooks in `actions/`.
#[cfg(test)]
pub fn for_test(base_url: &str) -> Arc<Upstream> {
    let cfg = Config::get();
    Arc::new(Upstream {
        client: Client::builder()
            .pool_idle_timeout(Duration::from_secs(cfg.pool_idle_timeout_s))
            .pool_max_idle_per_host(cfg.pool_max_idle_per_host)
            .build()
            .expect("test reqwest::Client"),
        base_url: base_url.trim_end_matches('/').to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_test_strips_trailing_slash() {
        let up = for_test("http://127.0.0.1:9999/");
        assert!(!up.base_url.ends_with('/'));
    }

    #[test]
    fn upstream_status_strings_are_stable() {
        assert_eq!(UpstreamStatus::Ok.as_str(), "ok");
        assert_eq!(UpstreamStatus::Unreachable.as_str(), "unreachable");
        assert_eq!(UpstreamStatus::Timeout.as_str(), "timeout");
    }

    #[tokio::test]
    async fn probe_ok_reports_model_count() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "a"}, {"name": "b"}, {"name": "c"}]
            })))
            .mount(&server)
            .await;
        let up = for_test(&server.uri());
        let probe = up.probe(2).await;
        assert_eq!(probe.status, UpstreamStatus::Ok);
        assert_eq!(probe.models, Some(3));
        // A reachable upstream reports a (small, local) latency.
        assert!(probe.latency_ms.is_some(), "ok probe must report latency");
    }

    #[tokio::test]
    async fn probe_down_upstream_is_a_down_state_never_ok() {
        // Bind a port, capture it, then drop the listener so nothing is
        // listening. Whether Windows answers a dead loopback port with an
        // RST (→ unreachable) or silently drops the SYN until our deadline
        // (→ timeout) is environment-dependent, so assert the meaningful
        // contract: a down upstream is never `Ok` and carries no model
        // count. The specific unreachable/timeout mappings are pinned by
        // `probe_non_2xx_is_unreachable` and `probe_timeout_maps_to_timeout_status`.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let up = for_test(&format!("http://127.0.0.1:{port}"));
        let probe = up.probe(2).await;
        assert_ne!(probe.status, UpstreamStatus::Ok);
        assert_eq!(probe.models, None);
        // No meaningful latency for a down upstream.
        assert_eq!(probe.latency_ms, None);
    }

    #[tokio::test]
    async fn probe_timeout_maps_to_timeout_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(1500))
                    .set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;
        let up = for_test(&server.uri());
        // 1s deadline fires before the mock's 1.5s delay → timeout.
        let probe = up.probe(1).await;
        assert_eq!(probe.status, UpstreamStatus::Timeout);
    }

    #[tokio::test]
    async fn probe_non_2xx_is_unreachable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&server)
            .await;
        let up = for_test(&server.uri());
        let probe = up.probe(2).await;
        assert_eq!(probe.status, UpstreamStatus::Unreachable);
    }
}
