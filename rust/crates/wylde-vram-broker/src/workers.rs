//! Background async tasks: reaper, Ollama poller, manifest-state writer.
//!
//! Rust port of `Core/resource_monitor/broker/workers.py`. The Python module
//! spawned three `threading.Thread` daemons; the Rust port spawns three
//! tokio tasks driven by a single shutdown notifier. Failure modes match the
//! Python side — every loop swallows transient errors and logs.

use std::sync::OnceLock;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::model_cache::model_cache;
use crate::registry::{refresh_nvml, refresh_sysinfo, registry, Lease};
use crate::time::now_secs;

/// Snapshot of running background-task handles. Owned by the singleton
/// returned from [`threads`] so `service::install` can be called more than
/// once without spawning duplicate workers.
pub struct Threads {
    reaper: Option<JoinHandle<()>>,
    ollama: Option<JoinHandle<()>>,
    manifest: Option<JoinHandle<()>>,
    pub(crate) stop: std::sync::Arc<Notify>,
    pub(crate) running: bool,
}

impl Threads {
    fn new() -> Self {
        Self {
            reaper: None,
            ollama: None,
            manifest: None,
            stop: std::sync::Arc::new(Notify::new()),
            running: false,
        }
    }
}

fn threads_cell() -> &'static Mutex<Threads> {
    static T: OnceLock<Mutex<Threads>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Threads::new()))
}

fn threads() -> MutexGuard<'static, Threads> {
    threads_cell().lock().expect("worker threads poisoned")
}

// ── Ollama poller ─────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct OllamaPsModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size_vram: Option<u64>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct OllamaPs {
    #[serde(default)]
    models: Vec<OllamaPsModel>,
}

/// Query Ollama `/api/ps` and produce synthetic leases for each running
/// model. Failures are swallowed (Ollama being down is a normal state).
pub async fn poll_ollama() -> Vec<Lease> {
    let cfg = Config::get();
    let url = format!("{}/api/ps", cfg.ollama_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let body = match client.get(&url).send().await {
        Ok(resp) => match resp.text().await {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    let parsed: OllamaPs = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let now = now_secs();
    parsed
        .models
        .into_iter()
        .filter_map(|m| {
            let name = m
                .name
                .clone()
                .or(m.model.clone())
                .unwrap_or_else(|| "unknown".into());
            // `/api/ps` reports the total resident size in `size`, and the
            // VRAM-resident portion in `size_vram`. When `size > size_vram`
            // the model has spilled — the remainder lives in DRAM. Some
            // older Ollama builds omit `size_vram`; in that case we treat
            // everything as VRAM (the safest assumption, matching the
            // pre-Phase-0.5 behaviour).
            let vram_only = m.size_vram.is_some() && m.size.is_none();
            let size_vram = m.size_vram.unwrap_or(0);
            let size_total = m.size.unwrap_or(size_vram);
            if size_total == 0 && size_vram == 0 {
                return None;
            }
            let (vram, dram) = if vram_only {
                (size_vram, 0u64)
            } else {
                let vr = size_vram.min(size_total);
                (vr, size_total.saturating_sub(vr))
            };
            Some(Lease {
                lease_id: format!("ollama:{}", uuid::Uuid::new_v4().simple()),
                service: "ollama".into(),
                model: name,
                bytes: vram,
                dram_bytes: dram,
                priority: 100,
                granted_at: now,
                // Synthetic leases are rebuilt each poll; this only exists
                // so the shape matches real leases.
                expires_at: now + 3600.0,
                heartbeat_at: now,
                pid: 0,
                synthetic: true,
                estimated: false,
                client_nonce: String::new(),
            })
        })
        .collect()
}

// ── Loop bodies ────────────────────────────────────────────────────────

async fn reaper_loop(stop: std::sync::Arc<Notify>) {
    let cfg = Config::get();
    let cache_prune_every_n = (60.0 / cfg.reaper_poll_s.max(1.0)).max(1.0) as u64;
    let mut tick: u64 = 0;
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = tokio::time::sleep(Duration::from_secs_f64(cfg.reaper_poll_s)) => {}
        }
        refresh_nvml();
        refresh_sysinfo();
        for lease in registry().reap_expired() {
            let short = lease.lease_id.get(..8).unwrap_or(&lease.lease_id);
            tracing::info!(
                "vram_broker: reaped expired lease {} ({}/{} {} bytes)",
                short,
                lease.service,
                lease.model,
                lease.bytes
            );
        }
        tick += 1;
        if tick % cache_prune_every_n == 0 {
            let pruned = model_cache().prune();
            if pruned > 0 {
                tracing::debug!("vram_broker: pruned {} stale model-cache entries", pruned);
            }
        }
    }
}

async fn ollama_loop(stop: std::sync::Arc<Notify>) {
    let cfg = Config::get();
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = tokio::time::sleep(Duration::from_secs_f64(cfg.ollama_poll_s)) => {}
        }
        let leases = poll_ollama().await;
        registry().replace_synthetic("ollama", leases);
    }
}

async fn manifest_loop(stop: std::sync::Arc<Notify>) {
    let cfg = Config::get();
    // Write once immediately so the GUI sees something before the first
    // poll interval elapses.
    write_state();
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = tokio::time::sleep(Duration::from_secs_f64(cfg.manifest_poll_s)) => {}
        }
        write_state();
    }
}

fn write_state() {
    let cfg = Config::get();
    let snapshot = state_snapshot();
    if let Some(parent) = cfg.state_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::debug!("vram_broker: state dir create failed: {e}");
            return;
        }
    }
    let tmp = cfg.state_path.with_extension("tmp");
    let body = match serde_json::to_string_pretty(&snapshot) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("vram_broker: state serialize failed: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(&tmp, body.as_bytes()) {
        tracing::debug!("vram_broker: state tmp write failed: {e}");
        let _ = std::fs::remove_file(&tmp); // wylde-check: discard-result-ok
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &cfg.state_path) {
        tracing::debug!("vram_broker: state rename failed: {e}");
        let _ = std::fs::remove_file(&tmp); // wylde-check: discard-result-ok
    }
}

/// Produce the JSON snapshot that backs `vram.state()` and the state file.
pub fn state_snapshot() -> Value {
    let now = now_secs();
    let leases = registry().all_leases();
    let mut by_service: std::collections::HashMap<String, serde_json::Map<String, Value>> =
        std::collections::HashMap::new();
    for lease in &leases {
        let entry = by_service.entry(lease.service.clone()).or_insert_with(|| {
            let mut m = serde_json::Map::new();
            m.insert("service".into(), Value::String(lease.service.clone()));
            m.insert("bytes".into(), json!(0u64));
            m.insert("dram_bytes".into(), json!(0u64));
            m.insert("count".into(), json!(0u64));
            m.insert("priority".into(), json!(lease.priority));
            m.insert("synthetic".into(), Value::Bool(lease.synthetic));
            m
        });
        let prev_bytes = entry.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        entry.insert("bytes".into(), json!(prev_bytes + lease.bytes));
        let prev_dram = entry
            .get("dram_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        entry.insert("dram_bytes".into(), json!(prev_dram + lease.dram_bytes));
        let prev_count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        entry.insert("count".into(), json!(prev_count + 1));
        let prev_pri = entry.get("priority").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        entry.insert("priority".into(), json!(prev_pri.max(lease.priority)));
    }
    let total = registry().total();
    let reserved = registry().reserved_total();
    let nvml_ts = registry().nvml_last_update();
    let total_dram = registry().total_dram();
    let actual_used_dram = registry().actual_used_dram();
    let reserved_dram = registry().reserved_dram_total();
    let sysinfo_ts = registry().sysinfo_last_update();
    let cache_entries = model_cache().all();
    let mut by_service_arr: Vec<Value> = by_service.into_values().map(Value::Object).collect();
    by_service_arr.sort_by_key(|v| -(v["priority"].as_i64().unwrap_or(0)));

    let cfg = Config::get();
    let cache_ttl = cfg.model_cache_ttl_s;
    let mut entries_sorted = cache_entries;
    entries_sorted.sort_by(|a, b| {
        b.last_used
            .partial_cmp(&a.last_used)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let entries_arr: Vec<Value> = entries_sorted
        .into_iter()
        .map(|e| {
            let warm_for = (cache_ttl - (now - e.last_used)).max(0.0);
            json!({
                "service": e.service,
                "model": e.model,
                "bytes": e.bytes,
                "last_used": e.last_used,
                "warm_for": warm_for,
            })
        })
        .collect();

    let nvml_fresh = if nvml_ts > 0.0 {
        Value::from(now - nvml_ts)
    } else {
        Value::Null
    };
    let sysinfo_fresh = if sysinfo_ts > 0.0 {
        Value::from(now - sysinfo_ts)
    } else {
        Value::Null
    };

    json!({
        "generated_at": now,
        "gpu": {
            "total_bytes": total,
            "actual_used_bytes": registry().actual_used(),
            "reserved_bytes": reserved,
            "free_for_grant": registry().free_for_grant(),
            "safety_margin": cfg.safety_margin,
            "name": registry().gpu_name(),
            "nvml_fresh_s": nvml_fresh,
        },
        // Phase 0.5: DRAM accounting. `reserved_dram_bytes` is the sum of
        // `dram_bytes` across all leases (spillover + Ollama-reported
        // overflow). `actual_used_bytes` comes from `sysinfo` and is the
        // ground-truth live usage, including processes outside the broker.
        "system_memory": {
            "total_bytes": total_dram,
            "actual_used_bytes": actual_used_dram,
            "reserved_dram_bytes": reserved_dram,
            "free_for_grant_dram": registry().free_for_grant_dram(),
            "safety_margin": cfg.dram_safety_margin,
            "sysinfo_fresh_s": sysinfo_fresh,
        },
        "leases": leases.iter().map(|l| serde_json::to_value(l).unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "by_service": by_service_arr,
        "model_cache": {
            "ttl_s": cache_ttl,
            "entries": entries_arr,
        },
        "config": {
            "safety_margin_bytes": cfg.safety_margin,
            "dram_safety_margin_bytes": cfg.dram_safety_margin,
            "enable_spillover": cfg.enable_spillover,
            "default_ttl": cfg.default_ttl,
            "ollama_poll_s": cfg.ollama_poll_s,
            "grace_period_s": cfg.grace_period_s,
            "model_cache_ttl_s": cache_ttl,
        },
    })
}

// ── Bootstrap / teardown ──────────────────────────────────────────────

/// Spawn the three background tasks. Idempotent: subsequent calls are
/// no-ops so `service::install` can be invoked safely from boot wrappers.
pub fn start_background() {
    let mut t = threads();
    if t.running {
        return;
    }
    t.running = true;
    let stop = t.stop.clone();
    let cfg = Config::get();
    let s1 = stop.clone();
    let s2 = stop.clone();
    t.reaper = Some(tokio::spawn(async move { reaper_loop(s1).await }));
    t.ollama = Some(tokio::spawn(async move { ollama_loop(s2).await }));
    t.manifest = Some(tokio::spawn(async move { manifest_loop(stop).await }));
    tracing::info!(
        "vram_broker: background tasks started (reaper {:.1}s, ollama {:.1}s, manifest {:.1}s)",
        cfg.reaper_poll_s,
        cfg.ollama_poll_s,
        cfg.manifest_poll_s,
    );
}

/// Signal every background loop to stop. Aborts handles to ensure the next
/// `start_background` call binds fresh tasks. Matches the Python `_reset`
/// semantics (swap in a clean stop event).
pub fn reset() {
    let mut t = threads();
    t.stop.notify_waiters();
    for h in [t.reaper.take(), t.ollama.take(), t.manifest.take()]
        .into_iter()
        .flatten()
    {
        h.abort();
    }
    t.stop = std::sync::Arc::new(Notify::new());
    t.running = false;
}

/// Trigger a single shutdown notification without resetting state. Used by
/// `service::stop` from the signal handler.
pub fn signal_stop() {
    let t = threads();
    t.stop.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::registry;
    use crate::test_lock::guard;

    const GB: u64 = 1024 * 1024 * 1024;

    fn fresh() {
        registry().reset();
        registry().set_gpu(16 * GB, 0, "TestGPU");
        model_cache().reset();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_has_expected_shape() {
        let _g = guard().await;
        fresh();
        let v = state_snapshot();
        assert_eq!(v["gpu"]["total_bytes"], 16 * GB);
        assert_eq!(v["gpu"]["reserved_bytes"], 0);
        assert!(v["leases"].is_array());
        assert!(v["by_service"].is_array());
        assert!(v["model_cache"]["entries"].is_array());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn by_service_sorted_priority_desc() {
        let _g = guard().await;
        fresh();
        registry().add(Lease {
            lease_id: "a".into(),
            service: "low".into(),
            model: "m".into(),
            bytes: GB,
            dram_bytes: 0,
            priority: 20,
            granted_at: 0.0,
            expires_at: 1e9,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: false,
            estimated: false,
            client_nonce: "".into(),
        });
        registry().add(Lease {
            lease_id: "b".into(),
            service: "high".into(),
            model: "m".into(),
            bytes: GB,
            dram_bytes: 0,
            priority: 100,
            granted_at: 0.0,
            expires_at: 1e9,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: false,
            estimated: false,
            client_nonce: "".into(),
        });
        let v = state_snapshot();
        let priorities: Vec<i64> = v["by_service"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["priority"].as_i64().unwrap())
            .collect();
        let mut sorted = priorities.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(priorities, sorted);
    }

    // ── Phase 0.5 tests ──────────────────────────────────────────────

    /// `state_snapshot()` exposes the new `system_memory` block and
    /// per-lease `dram_bytes` so the GUI can render spilled models.
    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_includes_system_memory_and_dram() {
        let _g = guard().await;
        fresh();
        registry().set_dram(32 * GB, 8 * GB);
        registry().add(Lease {
            lease_id: "spilled".into(),
            service: "ollama".into(),
            model: "big".into(),
            bytes: 12 * GB,
            dram_bytes: 4 * GB,
            priority: 100,
            granted_at: 0.0,
            expires_at: 1e9,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: true,
            estimated: false,
            client_nonce: "".into(),
        });

        let v = state_snapshot();
        let sm = &v["system_memory"];
        assert_eq!(sm["total_bytes"], 32 * GB);
        assert_eq!(sm["actual_used_bytes"], 8 * GB);
        assert_eq!(sm["reserved_dram_bytes"], 4 * GB);
        assert!(sm["safety_margin"].as_u64().unwrap() > 0);

        let lease = &v["leases"][0];
        assert_eq!(lease["bytes"], 12 * GB);
        assert_eq!(lease["dram_bytes"], 4 * GB);

        let bsvc = &v["by_service"][0];
        assert_eq!(bsvc["dram_bytes"], 4 * GB);
    }

    /// Synthetic-lease construction from `/api/ps` correctly splits the
    /// total resident size across VRAM and DRAM.
    #[test]
    fn poller_splits_vram_dram_from_size_fields() {
        // Simulate `/api/ps` payload — size_vram < size means spillover.
        let payload = serde_json::json!({
            "models": [
                {
                    "name": "gemma3:27b",
                    "size_vram": 10u64 * 1024 * 1024 * 1024,
                    "size":      15u64 * 1024 * 1024 * 1024,
                },
                {
                    "name": "qwen2.5:7b",
                    "size_vram": 5u64 * 1024 * 1024 * 1024,
                    "size":      5u64 * 1024 * 1024 * 1024,
                }
            ]
        });
        let parsed: OllamaPs = serde_json::from_value(payload).unwrap();

        let now = 0.0_f64;
        let mut spilled = None;
        let mut pure = None;
        for m in parsed.models {
            let name = m.name.clone().unwrap_or_default();
            let vram_only = m.size_vram.is_some() && m.size.is_none();
            let size_vram = m.size_vram.unwrap_or(0);
            let size_total = m.size.unwrap_or(size_vram);
            let (vram, dram) = if vram_only {
                (size_vram, 0u64)
            } else {
                let vr = size_vram.min(size_total);
                (vr, size_total.saturating_sub(vr))
            };
            if name == "gemma3:27b" {
                spilled = Some((vram, dram, now));
            } else if name == "qwen2.5:7b" {
                pure = Some((vram, dram));
            }
        }
        let (svram, sdram, _) = spilled.unwrap();
        assert_eq!(svram, 10 * GB);
        assert_eq!(sdram, 5 * GB);

        let (pvram, pdram) = pure.unwrap();
        assert_eq!(pvram, 5 * GB);
        assert_eq!(pdram, 0);
    }

    /// Once Ollama reports that a model has spilled (size > size_vram),
    /// `replace_synthetic` updates the lease — and **no** eviction is
    /// triggered. The model just shows up as spilled on the next poll.
    #[tokio::test(flavor = "current_thread")]
    async fn ollama_spillover_does_not_evict_existing_leases() {
        let _g = guard().await;
        fresh();
        registry().set_dram(32 * GB, 0);
        // Pre-existing real lease that we want NOT evicted.
        registry().add(Lease {
            lease_id: "real-low".into(),
            service: "wylde-caption".into(),
            model: "florence".into(),
            bytes: 2 * GB,
            dram_bytes: 0,
            priority: 40,
            granted_at: 0.0,
            expires_at: 1e9,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: false,
            estimated: false,
            client_nonce: "".into(),
        });
        // Pre-existing synthetic lease (Ollama observed; pure-VRAM).
        registry().add(Lease {
            lease_id: "syn-old".into(),
            service: "ollama".into(),
            model: "qwen2.5:14b".into(),
            bytes: 10 * GB,
            dram_bytes: 0,
            priority: 100,
            granted_at: 0.0,
            expires_at: 1e9,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: true,
            estimated: false,
            client_nonce: "".into(),
        });
        let before = registry().all_leases().len();

        // Ollama's next poll reports that the synthetic lease has
        // spilled: 8 GB VRAM resident, 4 GB DRAM.
        registry().replace_synthetic(
            "ollama",
            vec![Lease {
                lease_id: "syn-new".into(),
                service: "ollama".into(),
                model: "qwen2.5:14b".into(),
                bytes: 8 * GB,
                dram_bytes: 4 * GB,
                priority: 100,
                granted_at: 0.0,
                expires_at: 1e9,
                heartbeat_at: 0.0,
                pid: 0,
                synthetic: true,
                estimated: false,
                client_nonce: "".into(),
            }],
        );

        // The real low-priority lease is still here.
        assert!(registry().get("real-low").is_some());
        // The synthetic was swapped, not added on top.
        assert_eq!(registry().all_leases().len(), before);
        let new_syn = registry()
            .all_leases()
            .into_iter()
            .find(|l| l.synthetic)
            .unwrap();
        assert_eq!(new_syn.bytes, 8 * GB);
        assert_eq!(new_syn.dram_bytes, 4 * GB);
    }
}
