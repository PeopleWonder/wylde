//! Warm `reqwest::Client` for talking to the local Ollama daemon.
//!
//! One `Client` per process, shared via `Arc`. Per design doc §4:
//!   * `pool_idle_timeout(90s)`
//!   * `pool_max_idle_per_host(16)`
//!   * `tcp_keepalive(60s)`
//!   * no per-Client timeout; per-call `.timeout(...)` on the request
//!     builder instead so each action can pick its own deadline.

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
    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(Config::get().health_timeout_s))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("upstream returned {} for {url}", resp.status());
        }
        Ok(())
    }

    /// Issue a JSON request with the given method, path, body, and
    /// per-call timeout. Returns the raw `Response` so callers can pick
    /// JSON vs stream consumption per-action.
    pub async fn request(
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
                Upstream::new(cfg)
                    .expect("upstream reqwest::Client construction failed at boot"),
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
}
