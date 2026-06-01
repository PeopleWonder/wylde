//! Lease store, nvml accounting, and the registry-level reset hook.
//!
//! Rust port of `Core/resource_monitor/broker/registry.py`. The registry is
//! the single shared state of the broker: every action handler eventually
//! reads or mutates [`Registry`] through the process-wide [`registry`]
//! accessor.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::time::now_secs;

/// On-wire shape for a lease. Matches Python's `Lease.to_wire()` dataclass.
///
/// Since Phase 0.5 the broker accounts for system DRAM as well as VRAM.
/// `bytes` continues to mean **VRAM bytes** (preserved for backward compat
/// with Python clients and existing parity tests). `dram_bytes` is the
/// additional system-DRAM footprint when a model has spilled — 0 for pure
/// VRAM leases, which is every lease in the pre-spillover world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub lease_id: String,
    pub service: String,
    pub model: String,
    pub bytes: u64,
    /// System-DRAM portion of this lease, in bytes. 0 means pure-VRAM.
    /// When non-zero, the model has spilled — `bytes` is the VRAM resident
    /// portion and `dram_bytes` is the rest. Total footprint is
    /// `bytes + dram_bytes`.
    #[serde(default)]
    pub dram_bytes: u64,
    pub priority: i32,
    pub granted_at: f64,
    pub expires_at: f64,
    pub heartbeat_at: f64,
    #[serde(default)]
    pub pid: u32,
    /// Synthetic leases are produced by the Ollama poller — they reflect
    /// VRAM the broker did not hand out but should still account for.
    #[serde(default)]
    pub synthetic: bool,
    /// True when the lease's `bytes`/`dram_bytes` came from
    /// [`crate::policy::estimate_for`] rather than a precise caller-supplied
    /// number. The Ollama poller swaps an estimated lease for a real-numbers
    /// synthetic lease once the model actually loads.
    #[serde(default)]
    pub estimated: bool,
    #[serde(default)]
    pub client_nonce: String,
}

/// Thread-safe lease store + headroom accounting.
///
/// The nvml-reported free bytes drift from internal accounting (driver
/// overhead, un-brokered loads, allocator fragmentation), so the broker
/// grants against *reserved* bytes for predictability but exposes nvml
/// values separately so the GUI can show the divergence.
pub struct Registry {
    inner: Mutex<RegistryInner>,
}

struct RegistryInner {
    leases: HashMap<String, Lease>,
    by_nonce: HashMap<String, String>,
    total_bytes: u64,
    actual_used_bytes: u64,
    gpu_name: String,
    nvml_last_update: f64,
    /// Total system DRAM in bytes (from `sysinfo` at startup, refreshed on
    /// the reaper cadence). 0 means "not probed yet".
    total_dram_bytes: u64,
    /// Live system-DRAM usage from `sysinfo`. Mirrors `actual_used_bytes`
    /// for VRAM.
    actual_used_dram_bytes: u64,
    sysinfo_last_update: f64,
}

impl Registry {
    fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                leases: HashMap::new(),
                by_nonce: HashMap::new(),
                total_bytes: 0,
                actual_used_bytes: 0,
                gpu_name: String::new(),
                nvml_last_update: 0.0,
                total_dram_bytes: 0,
                actual_used_dram_bytes: 0,
                sysinfo_last_update: 0.0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RegistryInner> {
        self.inner.lock().expect("registry poisoned")
    }

    // ── nvml wrappers ─────────────────────────────────────────────────
    pub fn set_gpu(&self, total: u64, used: u64, name: &str) {
        let mut g = self.lock();
        g.total_bytes = total;
        g.actual_used_bytes = used;
        g.gpu_name = name.to_owned();
        g.nvml_last_update = now_secs();
    }

    pub fn total(&self) -> u64 {
        self.lock().total_bytes
    }

    pub fn actual_used(&self) -> u64 {
        self.lock().actual_used_bytes
    }

    pub fn gpu_name(&self) -> String {
        self.lock().gpu_name.clone()
    }

    pub fn nvml_last_update(&self) -> f64 {
        self.lock().nvml_last_update
    }

    pub fn reserved_total(&self) -> u64 {
        self.lock().leases.values().map(|l| l.bytes).sum()
    }

    /// Sum of DRAM portions across all leases (only spilled leases
    /// contribute; pure-VRAM leases have `dram_bytes = 0`).
    pub fn reserved_dram_total(&self) -> u64 {
        self.lock().leases.values().map(|l| l.dram_bytes).sum()
    }

    /// VRAM bytes we're willing to hand out right now.
    pub fn free_for_grant(&self) -> u64 {
        let g = self.lock();
        let reserved: u64 = g.leases.values().map(|l| l.bytes).sum();
        let margin = Config::get().safety_margin;
        g.total_bytes
            .saturating_sub(reserved)
            .saturating_sub(margin)
    }

    // ── sysinfo (DRAM) wrappers ───────────────────────────────────────
    pub fn set_dram(&self, total: u64, used: u64) {
        let mut g = self.lock();
        g.total_dram_bytes = total;
        g.actual_used_dram_bytes = used;
        g.sysinfo_last_update = now_secs();
    }

    pub fn total_dram(&self) -> u64 {
        self.lock().total_dram_bytes
    }

    pub fn actual_used_dram(&self) -> u64 {
        self.lock().actual_used_dram_bytes
    }

    pub fn sysinfo_last_update(&self) -> f64 {
        self.lock().sysinfo_last_update
    }

    /// DRAM bytes we're willing to hand out for spillover. Mirrors
    /// [`free_for_grant`] for VRAM.
    pub fn free_for_grant_dram(&self) -> u64 {
        let g = self.lock();
        let reserved: u64 = g.leases.values().map(|l| l.dram_bytes).sum();
        let margin = Config::get().dram_safety_margin;
        g.total_dram_bytes
            .saturating_sub(reserved)
            .saturating_sub(margin)
    }

    // ── lease CRUD ────────────────────────────────────────────────────
    pub fn find_by_nonce(&self, nonce: &str) -> Option<Lease> {
        if nonce.is_empty() {
            return None;
        }
        let g = self.lock();
        let lid = g.by_nonce.get(nonce)?;
        g.leases.get(lid).cloned()
    }

    pub fn add(&self, lease: Lease) {
        let mut g = self.lock();
        let nonce = lease.client_nonce.clone();
        let lid = lease.lease_id.clone();
        g.leases.insert(lid.clone(), lease);
        if !nonce.is_empty() {
            g.by_nonce.insert(nonce, lid);
        }
    }

    pub fn get(&self, lease_id: &str) -> Option<Lease> {
        self.lock().leases.get(lease_id).cloned()
    }

    pub fn remove(&self, lease_id: &str) -> Option<Lease> {
        let mut g = self.lock();
        let lease = g.leases.remove(lease_id)?;
        if !lease.client_nonce.is_empty() {
            g.by_nonce.remove(&lease.client_nonce);
        }
        Some(lease)
    }

    pub fn touch(&self, lease_id: &str, ttl: f64) -> Option<Lease> {
        let mut g = self.lock();
        let lease = g.leases.get_mut(lease_id)?;
        let now = now_secs();
        lease.heartbeat_at = now;
        lease.expires_at = now + ttl;
        Some(lease.clone())
    }

    pub fn all_leases(&self) -> Vec<Lease> {
        self.lock().leases.values().cloned().collect()
    }

    pub fn reap_expired(&self) -> Vec<Lease> {
        let now = now_secs();
        let mut removed = Vec::new();
        let mut g = self.lock();
        let expired_ids: Vec<String> = g
            .leases
            .iter()
            .filter(|(_, l)| !l.synthetic && now > l.expires_at)
            .map(|(k, _)| k.clone())
            .collect();
        for lid in expired_ids {
            if let Some(lease) = g.leases.remove(&lid) {
                if !lease.client_nonce.is_empty() {
                    g.by_nonce.remove(&lease.client_nonce);
                }
                removed.push(lease);
            }
        }
        removed
    }

    /// Atomically swap all synthetic leases for one service. Used by the
    /// Ollama poller — we don't track per-model identity across polls, we
    /// just rebuild the whole set each tick.
    pub fn replace_synthetic(&self, service: &str, new_leases: Vec<Lease>) {
        let mut g = self.lock();
        let synthetic_ids: Vec<String> = g
            .leases
            .iter()
            .filter(|(_, l)| l.synthetic && l.service == service)
            .map(|(k, _)| k.clone())
            .collect();
        for lid in synthetic_ids {
            if let Some(lease) = g.leases.remove(&lid) {
                if !lease.client_nonce.is_empty() {
                    g.by_nonce.remove(&lease.client_nonce);
                }
            }
        }
        for lease in new_leases {
            g.leases.insert(lease.lease_id.clone(), lease);
        }
    }

    /// Test-only: clear in place rather than rebinding so the global pointer
    /// returned by [`registry`] stays valid for re-bound test closures.
    pub fn reset(&self) {
        let mut g = self.lock();
        g.leases.clear();
        g.by_nonce.clear();
        g.total_bytes = 0;
        g.actual_used_bytes = 0;
        g.gpu_name.clear();
        g.nvml_last_update = 0.0;
        g.total_dram_bytes = 0;
        g.actual_used_dram_bytes = 0;
        g.sysinfo_last_update = 0.0;
    }
}

/// Process-wide registry. Lazy-init so config + tests can interleave.
pub fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(Registry::new)
}

// ── nvml bridge ────────────────────────────────────────────────────────
//
// `nvml-wrapper` is sync and holds Send/Sync NVML handles. We keep one
// process-wide handle behind a Mutex; failures are logged and turn into
// no-op refreshes so the broker still serves traffic on hosts without an
// NVIDIA GPU (or when the driver is mid-restart).

use std::sync::Mutex as StdMutex;

struct NvmlBridge {
    nvml: Option<nvml_wrapper::Nvml>,
}

fn nvml_bridge() -> &'static StdMutex<NvmlBridge> {
    static N: OnceLock<StdMutex<NvmlBridge>> = OnceLock::new();
    N.get_or_init(|| StdMutex::new(NvmlBridge { nvml: None }))
}

/// Initialize the NVML bridge. Returns `true` on success. Subsequent calls
/// after a successful init are no-ops. Failures are logged at warn and
/// return `false` — the broker proceeds without GPU info.
pub fn init_nvml() -> bool {
    let mut bridge = nvml_bridge().lock().expect("nvml bridge poisoned");
    if bridge.nvml.is_some() {
        return true;
    }
    match nvml_wrapper::Nvml::init() {
        Ok(n) => {
            bridge.nvml = Some(n);
            true
        }
        Err(e) => {
            tracing::warn!(
                "vram_broker: nvml unavailable, headroom accounting will use env total only: {e}"
            );
            false
        }
    }
}

// ── sysinfo bridge ─────────────────────────────────────────────────────
//
// System DRAM totals come from the `sysinfo` crate. The probe is cheap but
// not free, so we hold a process-wide `System` handle and refresh on the
// reaper cadence. Missing sysinfo support (extremely rare; would mean an
// unsupported OS) becomes a silent no-op — the registry just keeps the
// totals at 0 and spillover paths refuse cleanly.

struct SysinfoBridge {
    sys: sysinfo::System,
}

fn sysinfo_bridge() -> &'static StdMutex<SysinfoBridge> {
    static S: OnceLock<StdMutex<SysinfoBridge>> = OnceLock::new();
    S.get_or_init(|| {
        StdMutex::new(SysinfoBridge {
            sys: sysinfo::System::new(),
        })
    })
}

/// Refresh DRAM totals from `sysinfo`. Best-effort: a probe failure (rare;
/// would mean an unsupported platform) leaves the registry's previous
/// snapshot in place.
pub fn refresh_sysinfo() {
    let mut bridge = match sysinfo_bridge().lock() {
        Ok(g) => g,
        Err(e) => {
            tracing::debug!("vram_broker: sysinfo bridge poisoned: {e}");
            return;
        }
    };
    bridge.sys.refresh_memory();
    let total = bridge.sys.total_memory();
    let used = bridge.sys.used_memory();
    registry().set_dram(total, used);
}

/// Refresh registry totals from NVML's device-0 view. Best-effort: if NVML
/// hasn't been initialised, or the call fails, the registry keeps its
/// previous snapshot.
pub fn refresh_nvml() {
    let bridge = nvml_bridge().lock().expect("nvml bridge poisoned");
    let Some(nvml) = bridge.nvml.as_ref() else {
        return;
    };
    let device = match nvml.device_by_index(0) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("vram_broker: nvml device_by_index(0) failed: {e}");
            return;
        }
    };
    let name = device.name().unwrap_or_else(|_| String::from("unknown"));
    let mem = match device.memory_info() {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("vram_broker: nvml memory_info failed: {e}");
            return;
        }
    };
    registry().set_gpu(mem.total, mem.used, &name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lock::guard;

    fn fresh() -> &'static Registry {
        let r = registry();
        r.reset();
        r
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_get_remove_roundtrip() {
        let _g = guard().await;
        let r = fresh();
        let lease = Lease {
            lease_id: "L1".into(),
            service: "svc".into(),
            model: "m".into(),
            bytes: 1024,
            priority: 40,
            granted_at: 1.0,
            expires_at: 100.0,
            heartbeat_at: 1.0,
            pid: 0,
            synthetic: false,
            estimated: false,
            client_nonce: "n1".into(),
            dram_bytes: 0,
        };
        r.add(lease.clone());
        assert!(r.get("L1").is_some());
        assert_eq!(r.reserved_total(), 1024);
        assert_eq!(r.find_by_nonce("n1").unwrap().lease_id, "L1");
        let removed = r.remove("L1").unwrap();
        assert_eq!(removed.bytes, 1024);
        assert!(r.get("L1").is_none());
        assert!(r.find_by_nonce("n1").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reap_expired_skips_synthetic() {
        let _g = guard().await;
        let r = fresh();
        r.add(Lease {
            lease_id: "real-old".into(),
            service: "s".into(),
            model: "m".into(),
            bytes: 1,
            dram_bytes: 0,
            priority: 40,
            granted_at: 0.0,
            expires_at: 0.0,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: false,
            estimated: false,
            client_nonce: "".into(),
        });
        r.add(Lease {
            lease_id: "syn-old".into(),
            service: "ollama".into(),
            model: "m".into(),
            bytes: 1,
            dram_bytes: 0,
            priority: 100,
            granted_at: 0.0,
            expires_at: 0.0,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: true,
            estimated: false,
            client_nonce: "".into(),
        });
        let removed = r.reap_expired();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].lease_id, "real-old");
        assert!(r.get("syn-old").is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replace_synthetic_swaps_one_service() {
        let _g = guard().await;
        let r = fresh();
        r.add(Lease {
            lease_id: "ollama:a".into(),
            service: "ollama".into(),
            model: "m1".into(),
            bytes: 1,
            dram_bytes: 0,
            priority: 100,
            granted_at: 0.0,
            expires_at: 0.0,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: true,
            estimated: false,
            client_nonce: "".into(),
        });
        r.replace_synthetic(
            "ollama",
            vec![Lease {
                lease_id: "ollama:b".into(),
                service: "ollama".into(),
                model: "m2".into(),
                bytes: 2,
                dram_bytes: 0,
                priority: 100,
                granted_at: 0.0,
                expires_at: 0.0,
                heartbeat_at: 0.0,
                pid: 0,
                synthetic: true,
                estimated: false,
                client_nonce: "".into(),
            }],
        );
        assert!(r.get("ollama:a").is_none());
        assert!(r.get("ollama:b").is_some());
    }
}
