//! Env-driven configuration for wylde-ollama.
//!
//! Same shape as the broker's `config.rs` — read once at first access,
//! cached in a process-wide `OnceLock`. Mutating env after start does
//! not retroactively change behaviour.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL for the local Ollama daemon. `OLLAMA_URL`. Defaults to
    /// `http://127.0.0.1:11434`. Same env-var name the Python harness
    /// reads so a single export covers both paths during the strangler.
    pub ollama_url: String,

    /// Max idle connections kept warm per host in the shared
    /// `reqwest::Client` pool. Default 16 — enough for the 4-ish
    /// concurrent generates Ollama actually serves; the broker prevents
    /// more upstream.
    pub pool_max_idle_per_host: usize,

    /// How long an idle pooled connection survives. Default 90s — the
    /// reqwest default tuned a little lower for short bursts.
    pub pool_idle_timeout_s: u64,

    /// TCP keepalive on pool connections. Default 60s.
    pub tcp_keepalive_s: u64,

    // ── Per-action timeouts (match the existing Python defaults). ─────
    pub health_timeout_s: u64,
    pub list_models_timeout_s: u64,
    pub list_loaded_timeout_s: u64,
    pub show_timeout_s: u64,
    pub delete_timeout_s: u64,
    pub eject_timeout_s: u64,
    pub embed_timeout_s: u64,
    pub chat_timeout_s: u64,

    // ── VRAM lease defaults (per design doc §3). ──────────────────────
    /// Default priority for chat/chat_stream when caller doesn't pass
    /// one. 40 matches the wylde-caption tier in the broker rules; chat
    /// callers typically override to 60 (interactive) or 30 (background).
    pub default_chat_priority: i64,

    /// Default lease TTL in seconds. 60s matches the broker's default
    /// and lines up with heartbeat-at-TTL/3 = 20s heartbeat cadence.
    pub lease_ttl_s: f64,

    /// Heartbeat cadence for long-running streams. 25s — under the 30s
    /// chunk-heartbeat in the IPC streaming primitive so a server-side
    /// heartbeat and a lease heartbeat tick on different beats.
    pub lease_heartbeat_s: u64,

    /// Conservative multiplier for unknown-model VRAM estimates: the
    /// model's on-disk size in bytes × this factor. Master plan Q3.
    pub vram_estimate_multiplier: f64,

    /// Skip the broker entirely for `ollama.embed`. Default false — the
    /// broker's dedupe-by-nonce fast path makes per-call leases cheap.
    /// Set `WYLDE_OLLAMA_EMBED_SKIP_BROKER=1` to opt out.
    pub embed_skip_broker: bool,

    /// Service name for the broker we lease against. Lets tests retarget
    /// to a fake broker pipe.
    pub broker_service: String,

    pub wylde_root: PathBuf,
}

impl Config {
    fn load() -> Self {
        let ollama_url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
        let ollama_url = ollama_url.trim_end_matches('/').to_owned();

        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            ollama_url,
            pool_max_idle_per_host: env_usize("WYLDE_OLLAMA_POOL_MAX_IDLE", 16),
            pool_idle_timeout_s: env_u64("WYLDE_OLLAMA_POOL_IDLE_S", 90),
            tcp_keepalive_s: env_u64("WYLDE_OLLAMA_TCP_KEEPALIVE_S", 60),
            // Mirror Python's ollama_client.py per-call timeouts.
            health_timeout_s: env_u64("WYLDE_OLLAMA_HEALTH_TIMEOUT_S", 3),
            list_models_timeout_s: env_u64("WYLDE_OLLAMA_LIST_MODELS_TIMEOUT_S", 10),
            list_loaded_timeout_s: env_u64("WYLDE_OLLAMA_LIST_LOADED_TIMEOUT_S", 3),
            show_timeout_s: env_u64("WYLDE_OLLAMA_SHOW_TIMEOUT_S", 8),
            delete_timeout_s: env_u64("WYLDE_OLLAMA_DELETE_TIMEOUT_S", 10),
            eject_timeout_s: env_u64("WYLDE_OLLAMA_EJECT_TIMEOUT_S", 8),
            embed_timeout_s: env_u64("WYLDE_OLLAMA_EMBED_TIMEOUT_S", 30),
            chat_timeout_s: env_u64("WYLDE_OLLAMA_CHAT_TIMEOUT_S", 120),
            default_chat_priority: env_i64("WYLDE_OLLAMA_CHAT_PRIORITY", 40),
            lease_ttl_s: env_f64("WYLDE_OLLAMA_LEASE_TTL_S", 60.0),
            lease_heartbeat_s: env_u64("WYLDE_OLLAMA_LEASE_HEARTBEAT_S", 25),
            vram_estimate_multiplier: env_f64("WYLDE_OLLAMA_VRAM_ESTIMATE_MULT", 1.2),
            embed_skip_broker: env_bool("WYLDE_OLLAMA_EMBED_SKIP_BROKER", false),
            broker_service: std::env::var("WYLDE_OLLAMA_BROKER_SERVICE")
                .unwrap_or_else(|_| "wylde-vram-broker".to_owned()),
            wylde_root,
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_i64(name: &str, default: i64) -> i64 {
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
        let cfg = Config::load();
        assert!(!cfg.ollama_url.ends_with('/'));
        assert!(cfg.pool_max_idle_per_host > 0);
        assert!(cfg.chat_timeout_s > 0);
        assert!(cfg.lease_ttl_s > 0.0);
        assert!(cfg.lease_heartbeat_s > 0);
        assert!(cfg.vram_estimate_multiplier >= 1.0);
    }
}
