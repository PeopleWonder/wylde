//! Reservation policy: priority admission, preemption, eviction signalling.
//!
//! Rust port of `Core/resource_monitor/broker/policy.py`. [`try_grant`] is
//! the single entry point used by the `vram.reserve` action handler. The
//! Python sleep loops (`time.sleep`) become `tokio::time::sleep` so the
//! handler stays cooperatively schedulable.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::estimate::{estimate_for, Estimate};
use crate::model_cache::model_cache;
use crate::registry::{registry, Lease};
use crate::time::now_secs;

/// Inbound reservation request. Mirrors the Python `_try_grant(req)` body
/// shape. Missing fields fall back to the same defaults Python uses.
#[derive(Debug, Clone, Deserialize)]
pub struct GrantRequest {
    #[serde(default = "default_service")]
    pub service: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub bytes: i64,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "default_ttl")]
    pub ttl: f64,
    #[serde(default)]
    pub preempt: bool,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub client_nonce: String,
}

fn default_service() -> String {
    "unknown".into()
}
fn default_model() -> String {
    "unknown".into()
}
fn default_priority() -> i32 {
    40
}
fn default_ttl() -> f64 {
    Config::get().default_ttl
}

/// Outcome of [`try_grant`]. Mirrors Python's `{"ok": ..., "lease"/"error": ...}`
/// dict, modelled as a Rust enum so handlers can branch without scraping
/// string keys.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GrantResult {
    /// `{"ok": true, "lease": {...}, ...}`. Extra fields (`preempted`,
    /// `soft_eviction`, `dedup`) appear when relevant.
    Ok(Value),
    /// `{"ok": false, "error": {"code": ..., "message": ..., "details": ...}}`
    Err(Value),
}

impl GrantResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, GrantResult::Ok(_))
    }

    pub fn into_value(self) -> Value {
        match self {
            GrantResult::Ok(v) | GrantResult::Err(v) => v,
        }
    }
}

/// Core reservation logic. Async because preemption may wait on
/// [`tokio::time::sleep`] inside the grace-period and hard-evict loops.
pub async fn try_grant(req: GrantRequest) -> GrantResult {
    let cfg = Config::get();
    let priority = req.priority;
    let service = req.service.clone();
    let model = req.model.clone();
    let nonce = req.client_nonce.clone();

    // Phase 0.5: when `bytes` is missing or non-positive the broker
    // estimates rather than refusing. Callers that pass a real positive
    // number get the exact pre-Phase-0.5 behaviour — `estimate_for`
    // short-circuits on a positive hint.
    let (nbytes, estimated) = if req.bytes > 0 {
        (req.bytes as u64, false)
    } else {
        let Estimate { vram, .. } = estimate_for(&service, &model, None);
        if vram == 0 {
            return err("invalid_request", "bytes must be positive", None);
        }
        tracing::info!(
            "vram_broker: estimated footprint for service={} model={} vram={} (no precise hint)",
            service,
            model,
            vram
        );
        (vram, true)
    };

    // Sanity: total VRAM+DRAM must at least be able to fit the request.
    // The grant logic below will compute the actual spillover split.
    let total_vram = registry().total();
    let total_dram = registry().total_dram();
    let total_capacity = total_vram
        .saturating_sub(cfg.safety_margin)
        .saturating_add(if cfg.enable_spillover {
            total_dram.saturating_sub(cfg.dram_safety_margin)
        } else {
            0
        });
    if total_vram != 0 && nbytes > total_capacity {
        return err(
            "would_exceed_total",
            format!(
                "request {} exceeds total addressable memory {} \
                 (VRAM {} + DRAM {} minus safety margins)",
                nbytes, total_capacity, total_vram, total_dram
            ),
            Some(json!({
                "total_bytes": total_vram,
                "total_dram_bytes": total_dram,
                "requested_bytes": nbytes,
                "safety_margin": cfg.safety_margin,
                "dram_safety_margin": cfg.dram_safety_margin,
                "enable_spillover": cfg.enable_spillover,
            })),
        );
    }

    // Dedupe retries: a client that retries with the same nonce after a
    // successful grant whose response was lost gets the same lease back.
    if let Some(existing) = registry().find_by_nonce(&nonce) {
        let v = serde_json::to_value(&existing).unwrap_or(Value::Null);
        return GrantResult::Ok(json!({
            "ok": true,
            "lease": v,
            "dedup": true,
        }));
    }

    // Fast path: fits entirely in VRAM.
    if nbytes <= registry().free_for_grant() {
        let lease = grant_split(
            &service,
            &model,
            nbytes,
            0,
            priority,
            req.ttl,
            req.pid,
            &nonce,
            estimated,
        );
        return GrantResult::Ok(json!({
            "ok": true,
            "lease": lease_to_wire(&lease),
        }));
    }

    // Phase 0.5 spillover path: doesn't fit in VRAM, but fits in
    // VRAM+DRAM combined. Place as much in VRAM as we have headroom for,
    // remainder in DRAM. The lease holds both numbers. This path does NOT
    // evict — spillover is fine and intentional.
    if cfg.enable_spillover {
        let free_vram = registry().free_for_grant();
        let free_dram = registry().free_for_grant_dram();
        let want_dram = nbytes.saturating_sub(free_vram);
        if want_dram <= free_dram {
            let lease = grant_split(
                &service,
                &model,
                free_vram,
                want_dram,
                priority,
                req.ttl,
                req.pid,
                &nonce,
                estimated,
            );
            let short = lease.lease_id.get(..8).unwrap_or(&lease.lease_id);
            tracing::info!(
                "vram_broker: spilled grant service={} model={} vram={} dram={} lease={}",
                service,
                model,
                free_vram,
                want_dram,
                short,
            );
            return GrantResult::Ok(json!({
                "ok": true,
                "lease": lease_to_wire(&lease),
                "spilled": true,
            }));
        }
    }

    // Doesn't fit even with spillover. Build the blocker list (real leases
    // of strictly lower priority) to consider preemption — same code path
    // as pre-Phase-0.5; spillover does NOT add new eviction triggers.
    let mut blockers: Vec<Lease> = registry()
        .all_leases()
        .into_iter()
        .filter(|l| !l.synthetic && l.priority < priority)
        .collect();
    blockers.sort_by_key(|l| l.priority);

    let freeable: u64 = blockers.iter().map(|l| l.bytes).sum();
    let free_now = registry().free_for_grant();

    if freeable + free_now < nbytes {
        // Even evicting every lower-priority lease wouldn't help.
        return insufficient(nbytes, priority, all_blockers_view(), Vec::new());
    }
    if !req.preempt {
        return insufficient(nbytes, priority, all_blockers_view(), Vec::new());
    }

    // Preemption: graceful soft-evict first, then hard-evict survivors.
    let mut to_evict: Vec<Lease> = Vec::new();
    let mut accum = free_now;
    for lease in &blockers {
        if accum >= nbytes {
            break;
        }
        to_evict.push(lease.clone());
        accum += lease.bytes;
    }

    if cfg.grace_period_s > 0.0 {
        for lease in &to_evict {
            signal_soft_evict(lease, cfg.grace_period_s).await;
        }
        let deadline = now_secs() + cfg.grace_period_s;
        while now_secs() < deadline {
            if nbytes <= registry().free_for_grant() {
                let lease = grant_split(
                    &service, &model, nbytes, 0, priority, req.ttl, req.pid, &nonce, estimated,
                );
                return GrantResult::Ok(json!({
                    "ok": true,
                    "lease": lease_to_wire(&lease),
                    "preempted": to_evict.iter().map(|l| l.lease_id.clone()).collect::<Vec<_>>(),
                    "soft_eviction": true,
                }));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Hard evict anything still holding.
    for lease in &to_evict {
        if registry().get(&lease.lease_id).is_some() {
            signal_evict(lease).await;
        }
    }

    let hard_deadline = now_secs() + cfg.evict_timeout_s;
    while now_secs() < hard_deadline {
        if nbytes <= registry().free_for_grant() {
            let lease = grant_split(
                &service, &model, nbytes, 0, priority, req.ttl, req.pid, &nonce, estimated,
            );
            return GrantResult::Ok(json!({
                "ok": true,
                "lease": lease_to_wire(&lease),
                "preempted": to_evict.iter().map(|l| l.lease_id.clone()).collect::<Vec<_>>(),
                "soft_eviction": false,
            }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Preemption didn't free enough in time. Return structured error naming
    // the services that didn't yield, so the GUI can escalate to the user.
    let still_held: Vec<String> = to_evict
        .iter()
        .filter(|l| registry().get(&l.lease_id).is_some())
        .map(|l| l.service.clone())
        .collect();
    insufficient(nbytes, priority, all_blockers_view(), still_held)
}

/// Record a grant in the registry and the keep-warm cache. `vram_bytes`
/// goes into `bytes`; `dram_bytes` records the spillover portion (0 for
/// pure-VRAM grants). The keep-warm cache stores only the VRAM portion
/// since that's what subsequent grant decisions key off.
#[allow(clippy::too_many_arguments)]
pub fn grant_split(
    service: &str,
    model: &str,
    vram_bytes: u64,
    dram_bytes: u64,
    priority: i32,
    ttl: f64,
    pid: u32,
    nonce: &str,
    estimated: bool,
) -> Lease {
    let now = now_secs();
    let lease = Lease {
        lease_id: uuid::Uuid::new_v4().simple().to_string(),
        service: service.to_owned(),
        model: model.to_owned(),
        bytes: vram_bytes,
        dram_bytes,
        priority,
        granted_at: now,
        expires_at: now + ttl,
        heartbeat_at: now,
        pid,
        synthetic: false,
        estimated,
        client_nonce: nonce.to_owned(),
    };
    registry().add(lease.clone());
    model_cache().touch(service, model, vram_bytes, priority);
    let short = lease.lease_id.get(..8).unwrap_or(&lease.lease_id);
    tracing::info!(
        "vram_broker: grant service={} model={} vram={} dram={} priority={} \
         estimated={} lease={}",
        service,
        model,
        vram_bytes,
        dram_bytes,
        priority,
        estimated,
        short,
    );
    lease
}

/// Convenience wrapper for pure-VRAM grants. Kept for callers (and tests)
/// that pre-date Phase 0.5.
pub fn grant(
    service: &str,
    model: &str,
    nbytes: u64,
    priority: i32,
    ttl: f64,
    pid: u32,
    nonce: &str,
) -> Lease {
    grant_split(service, model, nbytes, 0, priority, ttl, pid, nonce, false)
}

pub fn lease_to_wire(lease: &Lease) -> Value {
    serde_json::to_value(lease).unwrap_or(Value::Null)
}

fn insufficient(
    nbytes: u64,
    priority: i32,
    blocker_snapshot: Vec<Value>,
    unresponsive: Vec<String>,
) -> GrantResult {
    let mut details = json!({
        "requested_bytes": nbytes,
        "requester_priority": priority,
        "free_bytes": registry().free_for_grant(),
        "total_bytes": registry().total(),
        "free_dram_bytes": registry().free_for_grant_dram(),
        "total_dram_bytes": registry().total_dram(),
        "blockers": blocker_snapshot,
    });
    if !unresponsive.is_empty() {
        if let Value::Object(ref mut m) = details {
            m.insert(
                "unresponsive_services".into(),
                Value::Array(unresponsive.into_iter().map(Value::String).collect()),
            );
        }
    }
    err(
        "insufficient_vram",
        format!("cannot satisfy {nbytes} bytes at priority {priority}"),
        Some(details),
    )
}

pub fn all_blockers_view() -> Vec<Value> {
    let mut leases = registry().all_leases();
    leases.sort_by_key(|l| -l.priority);
    leases
        .into_iter()
        .map(|l| {
            json!({
                "lease_id": l.lease_id,
                "service": l.service,
                "model": l.model,
                "bytes": l.bytes,
                "priority": l.priority,
                "synthetic": l.synthetic,
            })
        })
        .collect()
}

fn err(code: &str, message: impl Into<String>, details: Option<Value>) -> GrantResult {
    let mut e = json!({
        "code": code,
        "message": message.into(),
    });
    if let Some(d) = details {
        if let Value::Object(ref mut m) = e {
            m.insert("details".into(), d);
        }
    }
    GrantResult::Err(json!({
        "ok": false,
        "error": e,
    }))
}

/// Fire a HARD evict request at the lease's owning service. Best-effort —
/// services that haven't implemented the handler get a transport error and
/// the reaper will eventually time them out instead.
pub async fn signal_evict(lease: &Lease) {
    let timeout = Duration::from_secs_f64(Config::get().evict_timeout_s.min(2.0));
    let reply = wylde_shared::ipc::send(
        &lease.service,
        "/vram/evict",
        json!({"lease_id": lease.lease_id, "model": lease.model}),
        timeout,
    )
    .await;
    let short = lease.lease_id.get(..8).unwrap_or(&lease.lease_id);
    if reply.ok {
        tracing::info!(
            "vram_broker: HARD evict signal sent to {} lease={}",
            lease.service,
            short
        );
    } else {
        tracing::debug!(
            "vram_broker: hard evict signal to {} failed: {:?}",
            lease.service,
            reply.error
        );
    }
}

/// Send a graceful eviction request — the service is asked to finish its
/// current task and release within `grace_s` seconds. If the service has
/// not implemented the soft handler, falls back to the hard path.
pub async fn signal_soft_evict(lease: &Lease, grace_s: f64) {
    let timeout = Duration::from_secs_f64(Config::get().evict_timeout_s.min(2.0));
    let reply = wylde_shared::ipc::send(
        &lease.service,
        "/vram/please-evict",
        json!({"lease_id": lease.lease_id, "model": lease.model, "grace_s": grace_s}),
        timeout,
    )
    .await;
    if !reply.ok {
        tracing::debug!(
            "vram_broker: {} did not accept soft eviction; falling back to hard /vram/evict",
            lease.service
        );
        signal_evict(lease).await;
        return;
    }
    let short = lease.lease_id.get(..8).unwrap_or(&lease.lease_id);
    tracing::info!(
        "vram_broker: SOFT evict signalled to {} lease={} grace={:.1}s",
        lease.service,
        short,
        grace_s
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::registry;
    use crate::test_lock::guard;

    const GB: u64 = 1024 * 1024 * 1024;

    fn fresh_registry() {
        registry().reset();
        registry().set_gpu(16 * GB, 0, "TestGPU");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn grants_when_fits() {
        let _g = guard().await;
        fresh_registry();
        let r = try_grant(GrantRequest {
            service: "wylde-caption".into(),
            model: "m".into(),
            bytes: (4 * GB) as i64,
            priority: 40,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        assert!(r.is_ok(), "{:?}", r);
        let v = r.into_value();
        assert_eq!(v["lease"]["bytes"], 4 * GB);
        assert_eq!(v["lease"]["service"], "wylde-caption");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_when_exceeds_total() {
        let _g = guard().await;
        fresh_registry();
        let r = try_grant(GrantRequest {
            service: "wylde-trainer".into(),
            model: "big".into(),
            bytes: (20 * GB) as i64,
            priority: 20,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        assert!(!r.is_ok());
        let v = r.into_value();
        assert_eq!(v["error"]["code"], "would_exceed_total");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_when_no_headroom_no_preempt() {
        let _g = guard().await;
        fresh_registry();
        // Hold 13 GB at inference priority.
        let _ = try_grant(GrantRequest {
            service: "ollama".into(),
            model: "big-llm".into(),
            bytes: (13 * GB) as i64,
            priority: 100,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        // Trainer (20) asks for 4 GB without preempt.
        let r = try_grant(GrantRequest {
            service: "wylde-trainer".into(),
            model: "lora".into(),
            bytes: (4 * GB) as i64,
            priority: 20,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        assert!(!r.is_ok());
        let v = r.into_value();
        assert_eq!(v["error"]["code"], "insufficient_vram");
        let blockers = v["error"]["details"]["blockers"].as_array().unwrap();
        assert!(blockers.iter().any(|b| b["service"] == "ollama"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nonce_dedupes() {
        let _g = guard().await;
        fresh_registry();
        let req = GrantRequest {
            service: "wylde-caption".into(),
            model: "x".into(),
            bytes: GB as i64,
            priority: 40,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "abc".into(),
        };
        let r1 = try_grant(req.clone()).await.into_value();
        let r2 = try_grant(req).await.into_value();
        assert_eq!(r1["lease"]["lease_id"], r2["lease"]["lease_id"]);
        assert_eq!(registry().reserved_total(), GB);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preempt_refused_when_blockers_higher_priority() {
        let _g = guard().await;
        fresh_registry();
        let _ = try_grant(GrantRequest {
            service: "ollama".into(),
            model: "m".into(),
            bytes: (12 * GB) as i64,
            priority: 100,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        let r = try_grant(GrantRequest {
            service: "wylde-trainer".into(),
            model: "lora".into(),
            bytes: (4 * GB) as i64,
            priority: 20,
            ttl: 60.0,
            preempt: true,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        let v = r.into_value();
        assert_eq!(v["error"]["code"], "insufficient_vram");
    }

    // ── Phase 0.5: spillover, estimation, no-nuisance-eviction ──────────

    fn fresh_with_dram(vram_gb: u64, dram_gb: u64) {
        registry().reset();
        registry().set_gpu(vram_gb * GB, 0, "TestGPU");
        registry().set_dram(dram_gb * GB, 0);
    }

    /// Grant of a request that exceeds VRAM but fits combined VRAM+DRAM
    /// produces a spilled lease (dram_bytes > 0) and triggers NO eviction.
    #[tokio::test(flavor = "current_thread")]
    async fn spillover_grant_succeeds_when_fits_combined() {
        let _g = guard().await;
        fresh_with_dram(16, 32);
        // Hold 14 GiB pure-VRAM. With a 512 MiB safety margin, free VRAM
        // is about 1.5 GiB.
        let _ = try_grant(GrantRequest {
            service: "ollama".into(),
            model: "qwen2.5:14b".into(),
            bytes: (14 * GB) as i64,
            priority: 100,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "hold".into(),
        })
        .await;
        let blockers_before = registry().all_leases().len();

        // Caption asks for 4 GiB — doesn't fit in VRAM headroom but fits
        // in DRAM. Must succeed without preemption.
        let r = try_grant(GrantRequest {
            service: "wylde-caption".into(),
            model: "florence-2".into(),
            bytes: (4 * GB) as i64,
            priority: 40,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "spill".into(),
        })
        .await;
        assert!(r.is_ok(), "spillover grant should succeed: {:?}", r);
        let v = r.into_value();
        assert_eq!(v["spilled"], true);
        assert!(
            v["lease"]["dram_bytes"].as_u64().unwrap() > 0,
            "expected DRAM portion on spilled lease, got {}",
            v["lease"]
        );
        // Hard requirement: no eviction triggered. Blocker count unchanged.
        assert_eq!(registry().all_leases().len(), blockers_before + 1);
    }

    /// Even with DRAM budget exhausted, a request bigger than VRAM+DRAM
    /// combined is refused — and the error advertises both totals.
    #[tokio::test(flavor = "current_thread")]
    async fn refuses_when_exceeds_combined_capacity() {
        let _g = guard().await;
        fresh_with_dram(16, 4);
        let r = try_grant(GrantRequest {
            service: "wylde-trainer".into(),
            model: "huge".into(),
            bytes: (100 * GB) as i64,
            priority: 20,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        assert!(!r.is_ok());
        let v = r.into_value();
        assert_eq!(v["error"]["code"], "would_exceed_total");
        let d = &v["error"]["details"];
        assert_eq!(d["requested_bytes"], 100 * GB);
        assert_eq!(d["total_bytes"], 16 * GB);
        assert_eq!(d["total_dram_bytes"], 4 * GB);
    }

    /// When a caller omits `bytes`, the broker estimates from the
    /// keep-warm cache and grants — no refusal for "missing bytes".
    #[tokio::test(flavor = "current_thread")]
    async fn estimates_when_bytes_missing() {
        let _g = guard().await;
        fresh_with_dram(16, 16);
        crate::model_cache::model_cache().reset();
        crate::model_cache::model_cache().touch("svc", "known", 3 * GB, 40);

        let r = try_grant(GrantRequest {
            service: "svc".into(),
            model: "known".into(),
            bytes: 0, // no precise hint
            priority: 40,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        assert!(r.is_ok(), "estimation should let the grant proceed: {r:?}");
        let v = r.into_value();
        assert_eq!(v["lease"]["bytes"], 3 * GB);
        assert_eq!(v["lease"]["estimated"], true);
    }

    /// Pre-Phase-0.5 callers passing precise byte counts must not be
    /// affected by estimation logic.
    #[tokio::test(flavor = "current_thread")]
    async fn precise_bytes_does_not_get_estimated_flag() {
        let _g = guard().await;
        fresh_with_dram(16, 16);
        let r = try_grant(GrantRequest {
            service: "wylde-caption".into(),
            model: "florence-2".into(),
            bytes: (2 * GB) as i64,
            priority: 40,
            ttl: 60.0,
            preempt: false,
            pid: 0,
            client_nonce: "".into(),
        })
        .await;
        let v = r.into_value();
        assert_eq!(v["lease"]["estimated"], false);
        assert_eq!(v["lease"]["dram_bytes"], 0);
    }
}
