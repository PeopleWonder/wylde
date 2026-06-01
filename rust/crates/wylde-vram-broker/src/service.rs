//! Service entrypoint: register pipe action handlers and start workers.
//!
//! Rust port of `Core/resource_monitor/broker/service.py`. The Python
//! version wired Flask HTTP routes and bridged them onto a pipe via
//! `PipeServer(app)`; the Rust version skips the HTTP surface and exposes
//! the same operations directly as pipe-side actions on the shared
//! `wylde_shared::ipc` server.
//!
//! Action surface (one-to-one with the Python `/vram/*` routes):
//!
//! | action            | python route       | data shape                                       |
//! |-------------------|--------------------|--------------------------------------------------|
//! | `vram.reserve`    | `POST /vram/reserve`   | `{service,model,bytes,priority,ttl,preempt,pid,client_nonce}` |
//! | `vram.release`    | `POST /vram/release`   | `{lease_id}`                                     |
//! | `vram.heartbeat`  | `POST /vram/heartbeat` | `{lease_id, ttl}`                                |
//! | `vram.state`      | `GET  /vram/state`     | `{}`                                             |
//! | `vram.leases`     | `GET  /vram/leases`    | `{}`                                             |
//! | `vram.cache`      | `GET  /vram/cache`     | `{}`                                             |
//! | `vram.evict`      | `POST /vram/evict`     | `{lease_id}`                                     |

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, unregister_action, IpcError, Reply};

use crate::config::Config;
use crate::inventory;
use crate::model_cache::model_cache;
use crate::policy::{self, GrantRequest, GrantResult};
use crate::registry::{init_nvml, refresh_nvml, refresh_sysinfo, registry};
use crate::time::now_secs;
use crate::workers;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `vram.*` action on the process-wide pipe registry and
/// start the background workers. Idempotent — repeat calls are no-ops so
/// boot wrappers can call `install` once at startup and again after a
/// late NVML probe without double-registering.
pub fn install(gpu_available: bool) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    if gpu_available {
        init_nvml();
        refresh_nvml();
        // Populate DRAM totals at boot too, not just on the reaper's first
        // tick: a model larger than VRAM needs the spillover capacity to be
        // known when the very first `vram.reserve` lands, or it spuriously
        // hits `would_exceed_total`. The reaper keeps refreshing on its
        // cadence. Gated with the GPU probe so `install(false)` keeps tests
        // hermetic (they control DRAM via `registry().set_dram`).
        refresh_sysinfo();
    }

    register_action_with_meta(
        "vram.reserve",
        |payload: Value| async move { handle_reserve(payload).await },
        // Reply shape (lease): lease_id, service, model, bytes (VRAM portion),
        // dram_bytes (DRAM portion; 0 unless spilled), priority, granted_at,
        // expires_at, heartbeat_at, pid, synthetic, estimated, client_nonce.
        // Sibling fields when relevant: preempted, soft_eviction, dedup,
        // spilled. When `bytes` is omitted or <= 0 the broker estimates from
        // the keep-warm cache, observed synthetic leases, or name heuristics.
        "Request a VRAM lease; may spill into DRAM and/or preempt lower-priority holders.",
        "wylde_vram_broker::service",
    );
    register_action_with_meta(
        "vram.release",
        |payload: Value| async move { handle_release(payload).await },
        "Release a lease by id; idempotent. Reply: {ok, known, freed_bytes}.",
        "wylde_vram_broker::service",
    );
    register_action_with_meta(
        "vram.heartbeat",
        |payload: Value| async move { handle_heartbeat(payload).await },
        "Extend a lease's expiry by ttl seconds. Reply: {lease_id, expires_at}.",
        "wylde_vram_broker::service",
    );
    register_action_with_meta(
        "vram.state",
        |payload: Value| async move { handle_state(payload).await },
        // Snapshot shape:
        //   gpu: { total_bytes, actual_used_bytes, reserved_bytes,
        //          free_for_grant, safety_margin, name, nvml_fresh_s }
        //   system_memory: { total_bytes, actual_used_bytes,
        //          reserved_dram_bytes, free_for_grant_dram,
        //          safety_margin, sysinfo_fresh_s }
        //   leases: [Lease { ..., bytes, dram_bytes, estimated, ... }]
        //   by_service: [{ service, bytes, dram_bytes, count, priority,
        //          synthetic }]
        //   model_cache: { ttl_s, entries: [...] }
        //   config: { safety_margin_bytes, dram_safety_margin_bytes,
        //          enable_spillover, default_ttl, ollama_poll_s,
        //          grace_period_s, model_cache_ttl_s }
        "Snapshot of GPU/system memory totals, leases, model cache, and config.",
        "wylde_vram_broker::service",
    );
    register_action_with_meta(
        "vram.leases",
        |payload: Value| async move { handle_leases(payload).await },
        // Reply: {leases: [Lease]} — each lease carries `bytes` (VRAM) and
        // `dram_bytes` (DRAM portion; 0 for pure-VRAM leases).
        "List all live leases.",
        "wylde_vram_broker::service",
    );
    register_action_with_meta(
        "vram.cache",
        |payload: Value| async move { handle_cache(payload).await },
        "List the soft-LRU (service, model) keep-warm cache.",
        "wylde_vram_broker::service",
    );
    register_action_with_meta(
        "vram.evict",
        |payload: Value| async move { handle_evict(payload).await },
        "Force-drop a lease by id; does NOT signal the owning service.",
        "wylde_vram_broker::service",
    );

    // Phase 12.2 — host hardware inventory for the first-run LLM
    // bootstrap. Sampled live on each call (refreshes NVML + sysinfo);
    // the caller gets the same CPU/RAM/disk/GPU/NPU/OS shape regardless
    // of whether the wizard has run yet.
    register_action_with_meta(
        "system.inventory",
        |payload: Value| async move { handle_system_inventory(payload).await },
        // Reply shape: { cpu, memory_total_bytes, memory_available_bytes,
        // disks, gpus, npu, os } — see `inventory::Inventory`.
        "Snapshot of host hardware: CPU brand + cores, RAM, mounted disks, \
         NVIDIA GPUs (via NVML), NPU presence, and OS. Used by the \
         first-run bootstrap LLM to pick models that fit the box.",
        "wylde_vram_broker::service",
    );

    workers::start_background();
}

/// Signal background tasks to stop. Safe to call from a signal handler.
pub fn stop() {
    workers::signal_stop();
}

/// Test-only: reset every singleton in place. Mirrors Python's
/// `_reset_for_tests` — used between cases so each test sees a clean broker
/// without process restarts.
pub fn reset_for_tests() {
    stop();
    for action in [
        "vram.reserve",
        "vram.release",
        "vram.heartbeat",
        "vram.state",
        "vram.leases",
        "vram.cache",
        "vram.evict",
        "system.inventory",
    ] {
        unregister_action(action);
    }
    registry().reset();
    model_cache().reset();
    workers::reset();
    INSTALLED.store(false, Ordering::SeqCst);
}

// ── Action handlers ───────────────────────────────────────────────────

async fn handle_reserve(payload: Value) -> Reply {
    let req: GrantRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            return Reply::err(IpcError::new(
                "invalid_request",
                format!("reserve payload decode failed: {e}"),
            ));
        }
    };
    let result = policy::try_grant(req).await;
    match result {
        GrantResult::Ok(v) => {
            // The Python /vram/reserve route returned the bare lease dict on
            // success. Preserve that wire shape; surface `preempted` /
            // `soft_eviction` / `dedup` as sibling fields when present.
            let lease = v.get("lease").cloned().unwrap_or(Value::Null);
            let mut out = lease;
            if let Some(p) = v.get("preempted") {
                if let Value::Object(ref mut m) = out {
                    m.insert("preempted".into(), p.clone());
                }
            }
            if let Some(p) = v.get("soft_eviction") {
                if let Value::Object(ref mut m) = out {
                    m.insert("soft_eviction".into(), p.clone());
                }
            }
            if let Some(p) = v.get("dedup") {
                if let Value::Object(ref mut m) = out {
                    m.insert("dedup".into(), p.clone());
                }
            }
            Reply::ok(out)
        }
        GrantResult::Err(v) => {
            let err = v.get("error").cloned().unwrap_or(Value::Null);
            let code = err
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("unknown")
                .to_owned();
            let message = err
                .get("message")
                .and_then(|c| c.as_str())
                .unwrap_or("reserve failed")
                .to_owned();
            let details = err.get("details").cloned();
            let mut e = IpcError::new(code, message);
            e.details = details;
            Reply::err(e)
        }
    }
}

async fn handle_release(payload: Value) -> Reply {
    let lease_id = payload
        .get("lease_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    match registry().remove(&lease_id) {
        None => Reply::ok(json!({"ok": true, "known": false})),
        Some(lease) => {
            let short = lease.lease_id.get(..8).unwrap_or(&lease.lease_id);
            tracing::info!(
                "vram_broker: release lease={} service={} model={}",
                short,
                lease.service,
                lease.model
            );
            Reply::ok(json!({
                "ok": true,
                "known": true,
                "freed_bytes": lease.bytes,
            }))
        }
    }
}

async fn handle_heartbeat(payload: Value) -> Reply {
    let lease_id = payload
        .get("lease_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let ttl = payload
        .get("ttl")
        .and_then(|v| v.as_f64())
        .unwrap_or(Config::get().default_ttl);
    match registry().touch(&lease_id, ttl) {
        None => Reply::err(IpcError::new(
            "not_found",
            "lease not found or already reaped",
        )),
        Some(lease) => Reply::ok(json!({
            "lease_id": lease.lease_id,
            "expires_at": lease.expires_at,
        })),
    }
}

async fn handle_state(_payload: Value) -> Reply {
    refresh_nvml();
    Reply::ok(workers::state_snapshot())
}

async fn handle_leases(_payload: Value) -> Reply {
    let leases: Vec<Value> = registry()
        .all_leases()
        .iter()
        .map(|l| serde_json::to_value(l).unwrap_or(Value::Null))
        .collect();
    Reply::ok(json!({ "leases": leases }))
}

async fn handle_cache(_payload: Value) -> Reply {
    let cfg = Config::get();
    let mut entries = model_cache().all();
    entries.sort_by(|a, b| {
        b.last_used
            .partial_cmp(&a.last_used)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let now = now_secs();
    let entries_arr: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            let warm_for = (cfg.model_cache_ttl_s - (now - e.last_used)).max(0.0);
            json!({
                "service": e.service,
                "model": e.model,
                "bytes": e.bytes,
                "last_used": e.last_used,
                "warm_for": warm_for,
            })
        })
        .collect();
    Reply::ok(json!({
        "ttl_s": cfg.model_cache_ttl_s,
        "entries": entries_arr,
    }))
}

async fn handle_evict(payload: Value) -> Reply {
    let lease_id = payload
        .get("lease_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    match registry().remove(&lease_id) {
        None => Reply::err(IpcError::new("not_found", "lease not found")),
        Some(lease) => Reply::ok(json!({
            "ok": true,
            "freed_bytes": lease.bytes,
        })),
    }
}

async fn handle_system_inventory(_payload: Value) -> Reply {
    Reply::ok(inventory::handle_inventory())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lock::guard;

    const GB: u64 = 1024 * 1024 * 1024;

    fn fresh() {
        reset_for_tests();
        registry().set_gpu(16 * GB, 0, "TestGPU");
        install(false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reserve_returns_lease_shape() {
        let _g = guard().await;
        fresh();
        let reply = handle_reserve(json!({
            "service": "wylde-caption",
            "model": "florence-2",
            "bytes": 4 * GB,
            "priority": 40,
            "ttl": 60.0,
        }))
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["service"], "wylde-caption");
        assert_eq!(reply.data["bytes"], 4 * GB);
        assert!(reply.data["lease_id"].is_string());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn release_known_and_unknown() {
        let _g = guard().await;
        fresh();
        let granted = handle_reserve(json!({
            "service": "wylde-rag",  // wylde-check: dead-ref-ok
            "model": "reranker",
            "bytes": 2 * GB,
            "priority": 60,
        }))
        .await;
        let lid = granted.data["lease_id"].as_str().unwrap().to_owned();

        let r1 = handle_release(json!({"lease_id": lid.clone()})).await;
        assert!(r1.ok);
        assert_eq!(r1.data["known"], true);
        assert_eq!(r1.data["freed_bytes"], 2 * GB);

        let r2 = handle_release(json!({"lease_id": lid})).await;
        assert_eq!(r2.data["known"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn heartbeat_extends_and_404() {
        let _g = guard().await;
        fresh();
        let granted = handle_reserve(json!({
            "service": "wylde-caption",
            "model": "m",
            "bytes": GB,
            "priority": 40,
            "ttl": 5.0,
        }))
        .await;
        let lid = granted.data["lease_id"].as_str().unwrap().to_owned();
        let first_exp = granted.data["expires_at"].as_f64().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        let hb = handle_heartbeat(json!({"lease_id": lid, "ttl": 60.0})).await;
        assert!(hb.ok);
        assert!(hb.data["expires_at"].as_f64().unwrap() > first_exp);

        let miss = handle_heartbeat(json!({"lease_id": "nope"})).await;
        assert!(!miss.ok);
        assert_eq!(miss.error.unwrap().code, "not_found");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_includes_gpu_and_leases() {
        let _g = guard().await;
        fresh();
        let _ = handle_reserve(json!({
            "service": "wylde-caption",
            "model": "x",
            "bytes": 2 * GB,
            "priority": 40,
        }))
        .await;
        let s = handle_state(Value::Null).await;
        assert!(s.ok);
        assert_eq!(s.data["gpu"]["total_bytes"], 16 * GB);
        assert_eq!(s.data["gpu"]["reserved_bytes"], 2 * GB);
        let leases = s.data["leases"].as_array().unwrap();
        assert!(leases.iter().any(|l| l["service"] == "wylde-caption"));
        let priorities: Vec<i64> = s.data["by_service"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["priority"].as_i64().unwrap())
            .collect();
        let mut sorted = priorities.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(priorities, sorted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evict_unknown_is_not_found() {
        let _g = guard().await;
        fresh();
        let r = handle_evict(json!({"lease_id": "nope"})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
    }
}
