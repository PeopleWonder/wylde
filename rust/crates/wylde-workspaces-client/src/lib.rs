//! `wylde-workspaces-client` — the shared IPC client for the
//! `wylde-workspaces` service.
//!
//! Every consumer (the harness chat-turn driver, the GUI Workspaces panel,
//! the Chat composer) talks to the service through this crate instead of
//! hand-rolling the pipe. It hosts the scope v2 §7 failure-mode policy in
//! one place: per-verb timeout tiers ([`timeouts`]), retry-by-verb-shape
//! ([`retry`]), a per-pipe circuit breaker ([`circuit_breaker`]), a
//! read-through TTL cache ([`cache`]), per-consumer fallbacks ([`fallback`]),
//! and three-tier error classification ([`error`]). The per-verb knobs live
//! in the [`verbs`] table.
//!
//! **Slice 0a (this scaffold)** wires all of that infrastructure but exposes
//! exactly one verb — [`WorkspacesClient::ping`] — which proves the
//! end-to-end round-trip works. Later slices add a method per `workspaces.*`
//! verb; each is a thin wrapper over [`WorkspacesClient::call_verb`] plus a
//! one-line entry in the [`verbs`] table.

pub mod cache;
pub mod circuit_breaker;
pub mod error;
pub mod fallback;
pub mod retry;
pub mod timeouts;
pub mod transport;
pub mod verbs;

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::cache::VerbCache;
use crate::circuit_breaker::{BreakerDecision, CircuitBreaker};
use crate::error::WorkspacesClientError;

pub use crate::error::{ErrorTier, WorkspacesClientError as ClientError};

/// The `ping` reply payload: `{ok, service, version}`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PingResponse {
    pub ok: bool,
    pub service: String,
    pub version: String,
}

/// Shared client for the workspaces service.
///
/// Construct with [`WorkspacesClient::new`] (a pipe path) or
/// [`WorkspacesClient::for_service`] (a service name). One client owns one
/// circuit breaker + cache; share a client across a consumer's call sites so
/// the breaker state is coherent.
#[derive(Debug)]
pub struct WorkspacesClient {
    /// Bare service name handed to the shared transport (it re-applies the
    /// `wylde-` prefix when building the pipe path).
    service: String,
    breaker: CircuitBreaker,
    cache: VerbCache,
}

impl WorkspacesClient {
    /// Build a client pointing at the given pipe path (e.g.
    /// `\\.\pipe\wylde-workspaces`). The service name is derived from the
    /// path's final component.
    pub fn new(pipe_path: PathBuf) -> Self {
        Self::for_service(transport::service_name_from_pipe_path(&pipe_path))
    }

    /// Build a client for a service name (`wylde-workspaces`, or an isolated
    /// test name). Uses the default 5-failure / 30s breaker.
    pub fn for_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            breaker: CircuitBreaker::new(),
            cache: VerbCache::new(),
        }
    }

    /// Build a client with an explicit circuit breaker — for tests that need
    /// to tune the threshold/cooldown or pre-trip the breaker.
    pub fn with_breaker(service: impl Into<String>, breaker: CircuitBreaker) -> Self {
        Self {
            service: service.into(),
            breaker,
            cache: VerbCache::new(),
        }
    }

    /// The bare service name this client targets.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Borrow the circuit breaker (diagnostics / tests).
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// The Slice 0a verb: a no-op liveness round-trip. Proves transport +
    /// handshake + dispatch all work end-to-end.
    pub async fn ping(&self) -> Result<PingResponse, WorkspacesClientError> {
        let data = self.call_verb("ping", Value::Null, 1).await?;
        serde_json::from_value(data)
            .map_err(|e| WorkspacesClientError::decode(format!("ping reply: {e}")))
    }

    /// Drive one verb call through the full resilience pipeline: cache →
    /// breaker → timed transport attempt(s) with retry → breaker bookkeeping.
    ///
    /// `hops` only matters for per-hop-budget verbs (`symbol_context`); pass
    /// `1` for everything else. Returns the verb's raw `data` payload on
    /// success.
    pub async fn call_verb(
        &self,
        verb: &str,
        payload: Value,
        hops: u32,
    ) -> Result<Value, WorkspacesClientError> {
        let def = verbs::lookup(verb).ok_or_else(|| WorkspacesClientError::unknown_verb(verb))?;

        // 1. Read-through cache (verbs with a TTL only).
        let cache_key = VerbCache::key(verb, &payload);
        if def.cache_ttl.is_some() {
            if let Some(hit) = self.cache.get(&cache_key) {
                return Ok(hit);
            }
        }

        // 2. Circuit breaker — fail fast when open.
        if let BreakerDecision::Open = self.breaker.check(verb) {
            return Err(WorkspacesClientError::breaker_open(verb));
        }

        // 3. Timed attempt(s) with retry-on-transport-failure.
        let timeout = def.timeout.budget(hops);
        let max_attempts = def.retry.max_attempts();
        let mut last_err: Option<WorkspacesClientError> = None;

        for attempt in 1..=max_attempts {
            let reply = transport::call_action(&self.service, verb, payload.clone(), timeout).await;

            if reply.ok {
                self.breaker.record_success(verb);
                if let Some(ttl) = def.cache_ttl {
                    self.cache.put(cache_key, reply.data.clone(), ttl);
                }
                return Ok(reply.data);
            }

            let err = WorkspacesClientError::from_ipc(reply.error.unwrap_or_else(|| {
                wylde_shared::ipc::IpcError::new("unknown", "ok=false reply with no error body")
            }));

            // Application errors (no_action, bad_request, …) mean the service
            // is healthy — don't retry, don't trip the breaker.
            if !err.transport {
                return Err(err);
            }

            last_err = Some(err);
            if let Some(delay) = def.retry.backoff_delay(attempt) {
                tokio::time::sleep(delay).await;
            }
        }

        // Retries exhausted on a transport failure → count one failed
        // operation against the breaker and surface the last error.
        self.breaker.record_failure(verb);
        Err(last_err.unwrap_or_else(|| {
            WorkspacesClientError::from_ipc(wylde_shared::ipc::IpcError::new(
                "pipe_io",
                "transport failed with no recorded error",
            ))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_derives_service_from_pipe_path() {
        let c = WorkspacesClient::new(PathBuf::from(r"\\.\pipe\wylde-workspaces"));
        assert_eq!(c.service(), "wylde-workspaces");
    }

    #[test]
    fn for_service_keeps_name() {
        let c = WorkspacesClient::for_service("wylde-workspaces-test-xyz");
        assert_eq!(c.service(), "wylde-workspaces-test-xyz");
    }
}
