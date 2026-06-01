//! VRAM lease lifecycle for voice model weights.
//!
//! Lifted in shape from `wylde-ollama::lease` (RAII guard, heartbeat
//! task, broker-unreachable rewrap). Smaller surface — voice models
//! are loaded once at first-call and held for the service's lifetime,
//! so we only need a single `acquire` path, no per-call priority
//! override. Phase 0.5 broker DRAM accounting covers the NPU buffer
//! footprint (the broker doesn't distinguish NPU from GPU bytes — it
//! just accounts for resident model bytes).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;
use uuid::Uuid;
use wylde_shared::ipc::{call_action, IpcError};

use crate::config::Config;

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

/// Acquire a lease against the broker for a loaded voice model.
///
/// `model` is the HF repo id (e.g. `"openai/whisper-small"`) — used as
/// the broker's accounting key. `bytes_hint` is the on-disk model size
/// in bytes when known; the broker uses it directly when present and
/// falls back to its own estimator when `None`.
pub async fn acquire(model: &str, bytes_hint: Option<u64>) -> Result<Lease, IpcError> {
    let cfg = Config::get();
    let nonce = Uuid::new_v4().simple().to_string();

    let mut payload = json!({
        "service": "wylde-voice",
        "model": model,
        "priority": cfg.default_priority,
        "ttl": cfg.lease_ttl_s,
        "client_nonce": nonce,
    });
    if let Some(bytes) = bytes_hint {
        payload["bytes"] = Value::from(bytes);
    }

    let lease_value = match call_action(&cfg.broker_service, "vram.reserve", payload).await {
        Ok(v) => v,
        Err(e) => {
            if e.code == "vram_admission_denied" || e.code == "invalid_request" {
                return Err(e);
            }
            // Rewrap pipe-level errors so callers can distinguish "broker
            // refused" from "couldn't reach broker".
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
        ticker.tick().await; // discard immediate first-tick
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
                            "wylde-voice: lease {} heartbeat failed: {} ({})",
                            &lid_for_task[..lid_for_task.len().min(8)],
                            e.message,
                            e.code,
                        );
                    }
                }
            }
        }
    });

    Ok(Lease {
        lease_id,
        model: model.to_owned(),
        heartbeat_stop,
        released: false,
    })
}
