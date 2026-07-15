//! Environment-driven configuration constants for the VRAM broker.
//!
//! Rust port of `Core/resource_monitor/broker/config.py`. Values are read
//! once at first access via [`Config::load`] — env mutations after process
//! start do not retroactively change behaviour, matching the Python
//! module-import semantics.

use std::path::PathBuf;
use std::sync::OnceLock;

/// All env-derived constants in one snapshot. Use [`Config::get`] from any
/// module that needs a value; the first call populates the global cache.
#[derive(Debug, Clone)]
pub struct Config {
    /// Safety margin — never hand out the last N bytes. NVIDIA drivers
    /// allocate ~256MB of scratch for CUDA kernels that no reservation
    /// covers, so we hold a configurable buffer back from grants.
    pub safety_margin: u64,
    /// Same idea for system DRAM: when Ollama (or any callee) spills into
    /// DRAM, hold a buffer back so the broker never advertises every byte
    /// of system memory as grantable.
    pub dram_safety_margin: u64,
    /// Master switch. When false, the broker behaves exactly as it did
    /// before Phase 0.5 — pure-VRAM only, no spillover, no estimation
    /// fallback. Useful for triage when DRAM accounting needs to be ruled
    /// out as the cause of a regression.
    pub enable_spillover: bool,
    /// Conservative estimate for an unknown model's VRAM footprint. Used
    /// when [`crate::policy::estimate_for`] has no cache, no synthetic
    /// lease, and no name-heuristic match.
    pub estimate_default_vram: u64,
    /// Conservative estimate for an unknown model's DRAM footprint. Used
    /// alongside [`estimate_default_vram`] for true unknown models.
    pub estimate_default_dram: u64,
    pub default_ttl: f64,
    pub ollama_url: String,
    pub ollama_poll_s: f64,
    pub reaper_poll_s: f64,
    pub manifest_poll_s: f64,
    pub evict_timeout_s: f64,
    /// Soft-eviction grace period: when the broker decides a lease must
    /// yield, it sends `/vram/please-evict` first and gives the owner this
    /// many seconds to finish in-flight work before the hard `/vram/evict`
    /// kicks in. Set to 0 to disable graceful eviction entirely.
    pub grace_period_s: f64,
    /// Per-(service, model) "keep warm for N seconds" hint. When a lease
    /// releases, the broker records (service, model) in a soft-LRU.
    pub model_cache_ttl_s: f64,
    pub wylde_root: PathBuf,
    pub state_path: PathBuf,
}

impl Config {
    fn load() -> Self {
        let safety_margin = env_int("WYLDE_VRAM_SAFETY_MB", 512).saturating_mul(1024 * 1024) as u64;
        let dram_safety_margin =
            env_int("WYLDE_DRAM_SAFETY_MB", 2048).saturating_mul(1024 * 1024) as u64;
        let enable_spillover = env_bool("WYLDE_VRAM_ENABLE_SPILLOVER", true);
        let estimate_default_vram =
            env_int("WYLDE_VRAM_ESTIMATE_DEFAULT_MB", 4 * 1024).saturating_mul(1024 * 1024) as u64;
        let estimate_default_dram =
            env_int("WYLDE_DRAM_ESTIMATE_DEFAULT_MB", 0).saturating_mul(1024 * 1024) as u64;
        let default_ttl = env_f64("WYLDE_VRAM_TTL", 60.0);
        let ollama_url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
        let ollama_poll_s = env_f64("WYLDE_VRAM_OLLAMA_POLL", 5.0);
        let reaper_poll_s = env_f64("WYLDE_VRAM_REAPER_POLL", 2.0);
        let manifest_poll_s = env_f64("WYLDE_VRAM_MANIFEST_POLL", 2.0);
        let evict_timeout_s = env_f64("WYLDE_VRAM_EVICT_TIMEOUT", 3.0);
        let grace_period_s = env_f64("WYLDE_VRAM_GRACE_PERIOD", 10.0);
        let model_cache_ttl_s = env_f64("WYLDE_VRAM_MODEL_CACHE_TTL", 1800.0);

        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let state_path = wylde_root
            .join("data")
            .join("state")
            .join("vram-broker.json");

        Self {
            safety_margin,
            dram_safety_margin,
            enable_spillover,
            estimate_default_vram,
            estimate_default_dram,
            default_ttl,
            ollama_url,
            ollama_poll_s,
            reaper_poll_s,
            manifest_poll_s,
            evict_timeout_s,
            grace_period_s,
            model_cache_ttl_s,
            wylde_root,
            state_path,
        }
    }

    /// Process-wide snapshot. Cached on first call.
    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

fn env_int(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        // Don't assert specific values — env may be set by the harness —
        // just that the snapshot is well-formed.
        let cfg = Config::load();
        assert!(cfg.safety_margin > 0);
        assert!(cfg.default_ttl > 0.0);
        assert!(cfg.model_cache_ttl_s > 0.0);
        assert!(cfg.state_path.ends_with("vram-broker.json"));
    }
}
