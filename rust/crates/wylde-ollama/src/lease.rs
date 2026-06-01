//! VRAM lease lifecycle around inference calls.
//!
//! Per design doc §3 `wylde-ollama` owns the broker handshake — callers
//! don't think about VRAM. This module:
//!
//!   1. Computes a VRAM byte estimate for the model (passed-in → /api/ps →
//!      /api/show.size → on-disk size × multiplier).
//!   2. Calls `vram.reserve` on the broker.
//!   3. Hands out a [`Lease`] guard that holds the lease_id and a
//!      heartbeat task. Dropping the guard releases the lease.
//!
//! Why a guard rather than `release()` at every exit path: streaming
//! handlers have many error paths (network error mid-stream, decode
//! failure, client cancel). RAII makes them all converge on one cleanup.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;
use uuid::Uuid;
use wylde_shared::ipc::{call_action, IpcError};

use crate::config::Config;

/// Priority tier override on a per-call basis.
#[derive(Debug, Clone, Copy)]
pub enum Priority {
    /// Use the config default (`WYLDE_OLLAMA_CHAT_PRIORITY`, default 40).
    Default,
    Explicit(i64),
}

impl Priority {
    fn resolve(self) -> i64 {
        match self {
            Priority::Default => Config::get().default_chat_priority,
            Priority::Explicit(p) => p,
        }
    }
}

/// Per-action lease request.
#[derive(Debug, Clone)]
pub struct LeaseRequest {
    pub model: String,
    /// VRAM estimate in bytes. If `None`, the broker falls back to its
    /// own estimator (per `Config::estimate_default_vram`). Pass `Some`
    /// when you've already computed a tighter number from /api/show or
    /// /api/ps.
    pub bytes_hint: Option<u64>,
    pub priority: Priority,
    /// Idempotency nonce — collapse retry-storm requests for the same
    /// (service, model, nonce) into one lease. Defaults to a fresh UUID.
    pub nonce: Option<String>,
}

impl LeaseRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            bytes_hint: None,
            priority: Priority::Default,
            nonce: None,
        }
    }
}

/// RAII guard around a granted lease. Drop releases on a best-effort
/// basis: a tokio task is spawned because `Drop` is sync but
/// `vram.release` is async. The heartbeat task is aborted on drop too.
pub struct Lease {
    lease_id: String,
    model: String,
    heartbeat_stop: Arc<Notify>,
    released: bool,
}

impl Lease {
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Explicit release. Idempotent — re-calling after drop is a no-op.
    /// Use this on success paths where you want to know whether the
    /// release went through; the drop-time release is fire-and-forget.
    pub async fn release(mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.heartbeat_stop.notify_waiters();
        release_inner(&self.lease_id).await;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.heartbeat_stop.notify_waiters();
        let lid = self.lease_id.clone();
        tokio::spawn(async move {
            release_inner(&lid).await;
        });
    }
}

async fn release_inner(lease_id: &str) {
    let cfg = Config::get();
    let _ = call_action(
        &cfg.broker_service,
        "vram.release",
        json!({"lease_id": lease_id}),
    )
    .await;
}

/// Acquire a lease against the broker. On grant, spawn a background
/// heartbeat that ticks every `lease_heartbeat_s` until the [`Lease`] is
/// dropped or released.
///
/// Errors:
///   * `vram_admission_denied` — broker said no (with details about
///     priority/bytes/gpu_total). Propagate to the caller verbatim per
///     the design doc §3 error envelope.
///   * `broker_unreachable` — the broker pipe couldn't be reached at
///     all. The harness can choose whether to retry or fall through.
pub async fn acquire(req: LeaseRequest) -> Result<Lease, IpcError> {
    let cfg = Config::get();
    let nonce = req.nonce.unwrap_or_else(|| Uuid::new_v4().simple().to_string());

    let mut payload = json!({
        "service": "wylde-ollama",
        "model": req.model,
        "priority": req.priority.resolve(),
        "ttl": cfg.lease_ttl_s,
        "client_nonce": nonce,
    });
    if let Some(bytes) = req.bytes_hint {
        payload["bytes"] = Value::from(bytes);
    }

    let lease_value = match call_action(&cfg.broker_service, "vram.reserve", payload).await {
        Ok(v) => v,
        Err(e) => {
            // Broker-side errors: pass admission denials through; rewrap
            // transport failures so the caller can distinguish "broker
            // refused" from "couldn't reach broker".
            if e.code == "vram_admission_denied" || e.code == "invalid_request" {
                return Err(e);
            }
            if matches!(
                e.code.as_str(),
                "pipe_unavailable"
                    | "pipe_connect"
                    | "pipe_timeout"
                    | "pipe_io"
                    | "handshake_timeout"
                    | "handshake_io"
                    | "handshake_rejected"
                    | "ipc_disabled"
                    | "no_http_backend"
            ) {
                return Err(IpcError::new(
                    "broker_unreachable",
                    format!("vram-broker unreachable: {}", e.message),
                ));
            }
            return Err(e);
        }
    };

    let lease_id = lease_value
        .get("lease_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            IpcError::new(
                "broker_protocol",
                "vram.reserve reply missing lease_id field",
            )
        })?
        .to_owned();

    let heartbeat_stop = Arc::new(Notify::new());
    let stop_clone = heartbeat_stop.clone();
    let lid_for_task = lease_id.clone();
    let interval = Duration::from_secs(cfg.lease_heartbeat_s);
    let ttl = cfg.lease_ttl_s;
    let broker = cfg.broker_service.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; we want to wait one interval
        // before the first heartbeat (the reserve just happened).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = stop_clone.notified() => return,
                _ = ticker.tick() => {
                    let res = call_action(
                        &broker,
                        "vram.heartbeat",
                        json!({"lease_id": lid_for_task, "ttl": ttl}),
                    ).await;
                    if let Err(e) = res {
                        tracing::warn!(
                            "wylde-ollama: lease {} heartbeat failed: {} ({})",
                            &lid_for_task[..lid_for_task.len().min(8)],
                            e.message,
                            e.code,
                        );
                        // Don't bail — broker may briefly hiccup; next tick
                        // will retry. The lease's TTL is the safety net.
                    }
                }
            }
        }
    });

    Ok(Lease {
        lease_id,
        model: req.model,
        heartbeat_stop,
        released: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_resolution() {
        let cfg_default = Config::get().default_chat_priority;
        assert_eq!(Priority::Default.resolve(), cfg_default);
        assert_eq!(Priority::Explicit(60).resolve(), 60);
    }

    #[test]
    fn lease_request_defaults() {
        let r = LeaseRequest::new("qwen2.5:0.5b");
        assert_eq!(r.model, "qwen2.5:0.5b");
        assert!(r.bytes_hint.is_none());
        assert!(r.nonce.is_none());
    }
}
