//! Runtime configuration — Rust port of `Gateway/settings.py`.
//!
//! All knobs come from `WYLDE_GATEWAY_*` environment variables, matching
//! the Python pydantic-settings layout. Defaults are identical so a
//! Python→Rust cutover doesn't require changing any env-var values.

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

/// Process-wide settings snapshot. Frozen after construction so concurrent
/// readers don't need a lock — once the lifespan startup has run, the
/// cell is set for the rest of the process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySettings {
    pub host: String,
    pub port: u16,
    pub workers: u16,

    pub local_cidrs_csv: String,
    pub trust_forwarded_for: bool,

    pub rate_limit_per_minute: u32,

    pub audit_log_dir: PathBuf,
    pub audit_log_enabled: bool,

    pub cors_origins_csv: String,

    pub secrets_provider: String,
    pub secrets_strict_mode: bool,

    pub egress_kill_switch_init: bool,
}

impl GatewaySettings {
    /// Construct from `WYLDE_GATEWAY_*` env vars. Missing keys fall back to
    /// the same defaults the Python `GatewaySettings(BaseSettings)` class
    /// declares.
    pub fn from_env() -> Self {
        Self {
            host: env_string("WYLDE_GATEWAY_HOST", "127.0.0.1"),
            port: env_u16("WYLDE_GATEWAY_PORT", 8005),
            workers: env_u16("WYLDE_GATEWAY_WORKERS", 1),
            local_cidrs_csv: env_string(
                "WYLDE_GATEWAY_LOCAL_CIDRS_CSV",
                "127.0.0.1/32,::1/128,172.16.0.0/12,100.64.0.0/10",
            ),
            trust_forwarded_for: env_bool("WYLDE_GATEWAY_TRUST_FORWARDED_FOR", false),
            rate_limit_per_minute: env_u32("WYLDE_GATEWAY_RATE_LIMIT_PER_MINUTE", 1000),
            audit_log_dir: env_path("WYLDE_GATEWAY_AUDIT_LOG_DIR", default_audit_dir()),
            audit_log_enabled: env_bool("WYLDE_GATEWAY_AUDIT_LOG_ENABLED", true),
            cors_origins_csv: env_string(
                "WYLDE_GATEWAY_CORS_ORIGINS_CSV",
                // The Tauri tree was deleted in the GPUI cutover (slice 11) and
                // the gpui GUI talks over the named pipe, not a web origin — so
                // the old desktop-GUI (`tauri://localhost`) and Vite dev-server
                // (`*:1420`) origins are gone. Bare `http://localhost` stays for
                // any loopback browser client (extension handlers / panels).
                "http://localhost",
            ),
            secrets_provider: env_string("WYLDE_GATEWAY_SECRETS_PROVIDER", "file"),
            secrets_strict_mode: env_bool("WYLDE_GATEWAY_SECRETS_STRICT_MODE", false),
            egress_kill_switch_init: env_bool("WYLDE_GATEWAY_EGRESS_KILL_SWITCH_INIT", false),
        }
    }

    pub fn local_cidrs(&self) -> Vec<String> {
        self.local_cidrs_csv
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn cors_origins(&self) -> Vec<String> {
        self.cors_origins_csv
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

fn default_audit_dir() -> PathBuf {
    // The Python default resolves to `Gateway/logs/`. For the Rust binary
    // we anchor on `WYLDE_ROOT/Gateway/logs/` so both implementations
    // append to the same JSONL file during the strangler-fig phase.
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("Gateway").join("logs")
}

fn env_string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_u16(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}

fn env_path(key: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(key).map(PathBuf::from).unwrap_or(default)
}

// ── Process-wide cache ─────────────────────────────────────────────────

fn cell() -> &'static RwLock<Option<GatewaySettings>> {
    static CELL: OnceLock<RwLock<Option<GatewaySettings>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// Return the process-wide settings. Constructs the snapshot from env on
/// first call; subsequent calls share it. Equivalent to
/// `Gateway.settings.get_settings`.
pub fn get_settings() -> GatewaySettings {
    if let Some(s) = cell().read().expect("settings poisoned").as_ref() {
        return s.clone();
    }
    let fresh = GatewaySettings::from_env();
    let mut guard = cell().write().expect("settings poisoned");
    if guard.is_none() {
        tracing::info!(
            "gateway settings: host={} port={} local_cidrs={:?}",
            fresh.host,
            fresh.port,
            fresh.local_cidrs(),
        );
        *guard = Some(fresh.clone());
    }
    guard.as_ref().expect("just inserted").clone()
}

/// Clear the cached settings — for tests that mutate env mid-run.
pub fn reset_settings_cache() {
    *cell().write().expect("settings poisoned") = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults_when_no_env() {
        let s = GatewaySettings::from_env();
        // We can't guarantee the host default because the test runner may
        // have set WYLDE_GATEWAY_HOST. Assert the structural invariants
        // that don't depend on the environment.
        assert!(s.port > 0);
        assert!(!s.local_cidrs().is_empty());
    }

    #[test]
    fn cors_origins_splits_csv() {
        let mut s = GatewaySettings::from_env();
        s.cors_origins_csv = "a, b ,, c".into();
        assert_eq!(s.cors_origins(), vec!["a", "b", "c"]);
    }
}
