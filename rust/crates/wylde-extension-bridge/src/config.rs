//! Env-driven configuration for wylde-extension-bridge.
//!
//! Same shape as `wylde-ollama::config` — read once at first access,
//! cached in a process-wide `OnceLock`. Mutating env after start does
//! not retroactively change behaviour.

use std::path::PathBuf;
use std::sync::OnceLock;

/// MCP wire-spec version this host negotiates against (Q-E1 pin,
/// 2026-05-23).
pub const MCP_SPEC_VERSION: &str = "2025-11-25";

/// Prior MCP wire-spec version (N-1). Per per-extension compat policy:
/// host accepts N and N-1, rejects N+1 with a clear log line. Update
/// when bumping `MCP_SPEC_VERSION`.
pub const MCP_SPEC_VERSION_PREV: &str = "2025-06-18";

#[derive(Debug, Clone)]
pub struct Config {
    pub wylde_root: PathBuf,

    /// Where to scan for extension folders. Default `<wylde_root>/Extensions`,
    /// falling back to `<wylde_root>/Wylde/Extensions` (both layouts seen
    /// in the tree). `WYLDE_EXTENSIONS_DIR` overrides.
    pub extensions_dir: PathBuf,

    /// Seconds to wait for MCP `initialize` reply on a fresh spawn.
    /// `WYLDE_EXT_INIT_TIMEOUT_S`. Default 10.
    pub init_timeout_s: u64,

    /// Seconds between `ping` health-check ticks. `WYLDE_EXT_HEALTH_INTERVAL_S`.
    /// Default 30.
    pub health_interval_s: u64,

    /// Per-tool-call timeout in seconds. `WYLDE_EXT_TOOL_CALL_TIMEOUT_S`.
    /// Default 60.
    pub tool_call_timeout_s: u64,

    /// Cap on consecutive restart attempts before marking an extension
    /// `broken`. `WYLDE_EXT_RESTART_MAX_ATTEMPTS`. Default 5.
    pub restart_max_attempts: u32,

    /// Max backoff between restart attempts in seconds.
    /// `WYLDE_EXT_RESTART_MAX_BACKOFF_S`. Default 60.
    pub restart_max_backoff_s: u64,

    /// Spawn enabled extensions eagerly at startup. Disable for
    /// lazy-first-call spawning. `WYLDE_EXT_EAGER_SPAWN`. Default true.
    pub eager_spawn: bool,
}

impl Config {
    fn load() -> Self {
        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let extensions_dir = std::env::var_os("WYLDE_EXTENSIONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let primary = wylde_root.join("Wylde").join("Extensions");
                if primary.exists() {
                    primary
                } else {
                    wylde_root.join("Extensions")
                }
            });

        Self {
            wylde_root,
            extensions_dir,
            init_timeout_s: env_u64("WYLDE_EXT_INIT_TIMEOUT_S", 10),
            health_interval_s: env_u64("WYLDE_EXT_HEALTH_INTERVAL_S", 30),
            tool_call_timeout_s: env_u64("WYLDE_EXT_TOOL_CALL_TIMEOUT_S", 60),
            restart_max_attempts: env_u32("WYLDE_EXT_RESTART_MAX_ATTEMPTS", 5),
            restart_max_backoff_s: env_u64("WYLDE_EXT_RESTART_MAX_BACKOFF_S", 60),
            eager_spawn: env_bool("WYLDE_EXT_EAGER_SPAWN", true),
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::load();
        assert!(cfg.init_timeout_s > 0);
        assert!(cfg.health_interval_s > 0);
        assert!(cfg.restart_max_attempts > 0);
    }

    #[test]
    fn spec_versions_are_distinct() {
        assert_ne!(MCP_SPEC_VERSION, MCP_SPEC_VERSION_PREV);
    }
}
