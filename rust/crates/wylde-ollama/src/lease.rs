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
//!
//! ## Never preempts
//!
//! The reserve payload never sets the broker's `preempt` flag (it defaults
//! to `false`), so a lease from this service is granted only if it fits,
//! spills into DRAM, or is refused. It never evicts another holder's
//! lease. FIM admission builds on this guarantee.
//!
//! ## The [`Leaser`] seam
//!
//! Handlers that need to prove lease lifecycle in tests take an
//! `Arc<dyn Leaser>` instead of calling [`acquire`] directly: production
//! passes [`broker`], tests pass a counting fake.

use std::future::Future;
use std::pin::Pin;
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
    pub(crate) fn resolve(self) -> i64 {
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
    /// DRAM portion of the grant; non-zero means the broker could only
    /// admit the model by spilling part of it out of VRAM.
    dram_bytes: u64,
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

    /// Whether the grant spilled into DRAM (did not fit in free VRAM).
    pub fn spilled(&self) -> bool {
        self.dram_bytes > 0
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
    let nonce = req
        .nonce
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let payload = reserve_payload(&req, &nonce, cfg.lease_ttl_s);

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
    let dram_bytes = lease_value
        .get("dram_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);

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
        dram_bytes,
        heartbeat_stop,
        released: false,
    })
}

/// The `vram.reserve` payload. Deliberately carries no `preempt` key: the
/// broker defaults it to `false`, so our leases never evict another holder.
fn reserve_payload(req: &LeaseRequest, nonce: &str, ttl: f64) -> Value {
    let mut payload = json!({
        "service": "wylde-ollama",
        "model": req.model,
        "priority": req.priority.resolve(),
        "ttl": ttl,
        "client_nonce": nonce,
    });
    if let Some(bytes) = req.bytes_hint {
        payload["bytes"] = Value::from(bytes);
    }
    payload
}

/// A held lease, as a handler sees it. Dropping it releases the lease.
pub trait LeaseHold: Send {
    /// Whether the grant spilled into DRAM (see [`Lease::spilled`]).
    fn spilled(&self) -> bool;
}

impl LeaseHold for Lease {
    fn spilled(&self) -> bool {
        Lease::spilled(self)
    }
}

/// Boxed future returned by [`Leaser::acquire`].
pub type AcquireFuture = Pin<Box<dyn Future<Output = Result<Box<dyn LeaseHold>, IpcError>> + Send>>;

/// Acquires leases. Production uses [`broker`]; tests substitute a fake to
/// observe that every exit path releases what it acquired.
pub trait Leaser: Send + Sync {
    fn acquire(&self, req: LeaseRequest) -> AcquireFuture;
}

/// The real broker-backed [`Leaser`] — a thin wrapper over [`acquire`].
pub struct BrokerLeaser;

impl Leaser for BrokerLeaser {
    fn acquire(&self, req: LeaseRequest) -> AcquireFuture {
        Box::pin(async move {
            acquire(req)
                .await
                .map(|l| Box::new(l) as Box<dyn LeaseHold>)
        })
    }
}

/// The process-wide broker-backed [`Leaser`].
pub fn broker() -> Arc<dyn Leaser> {
    Arc::new(BrokerLeaser)
}

/// Test doubles for the [`Leaser`] seam.
#[cfg(test)]
pub mod testing {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use wylde_shared::ipc::IpcError;

    use super::{AcquireFuture, LeaseHold, LeaseRequest, Leaser};

    /// What [`FakeLeaser::acquire`] does.
    #[derive(Clone)]
    pub enum Outcome {
        /// Grant a lease; `spilled` marks it as a DRAM-spilled grant.
        Grant { spilled: bool },
        /// Fail with this error code (e.g. `broker_unreachable`).
        Fail(&'static str),
    }

    /// Counts acquisitions and releases and records each request's model
    /// and resolved priority.
    pub struct FakeLeaser {
        outcome: Outcome,
        acquired: AtomicUsize,
        released: Arc<AtomicUsize>,
        requests: Mutex<Vec<(String, i64)>>,
    }

    impl FakeLeaser {
        pub fn new(outcome: Outcome) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                acquired: AtomicUsize::new(0),
                released: Arc::new(AtomicUsize::new(0)),
                requests: Mutex::new(Vec::new()),
            })
        }
        pub fn acquired(&self) -> usize {
            self.acquired.load(Ordering::SeqCst)
        }
        pub fn released(&self) -> usize {
            self.released.load(Ordering::SeqCst)
        }
        pub fn priorities(&self) -> Vec<i64> {
            self.requests.lock().unwrap().iter().map(|r| r.1).collect()
        }
    }

    struct FakeHold {
        spilled: bool,
        released: Arc<AtomicUsize>,
    }

    impl LeaseHold for FakeHold {
        fn spilled(&self) -> bool {
            self.spilled
        }
    }

    impl Drop for FakeHold {
        fn drop(&mut self) {
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Leaser for FakeLeaser {
        fn acquire(&self, req: LeaseRequest) -> AcquireFuture {
            self.requests
                .lock()
                .unwrap()
                .push((req.model.clone(), req.priority.resolve()));
            let result: Result<Box<dyn LeaseHold>, IpcError> = match &self.outcome {
                Outcome::Grant { spilled } => {
                    self.acquired.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(FakeHold {
                        spilled: *spilled,
                        released: self.released.clone(),
                    }))
                }
                Outcome::Fail(code) => Err(IpcError::new(*code, "fake leaser failure")),
            };
            Box::pin(async move { result })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_payload_never_requests_preemption() {
        let mut req = LeaseRequest::new("qwen");
        req.bytes_hint = Some(1024);
        req.priority = Priority::Explicit(90);
        // A generated nonce, as in production (a literal trips CodeQL's
        // hard-coded-crypto-value rule; this is an idempotency key).
        let nonce = Uuid::new_v4().simple().to_string();
        let p = reserve_payload(&req, &nonce, 60.0);
        assert!(p.get("preempt").is_none(), "must never set preempt: {p}");
        assert_eq!(p["priority"], 90);
        assert_eq!(p["bytes"], 1024);
        assert_eq!(p["client_nonce"], nonce.as_str());
    }

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
