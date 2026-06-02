//! Env-driven configuration for wylde-ext-webcrawler.
//!
//! Same shape as `wylde-treesitter`'s `config.rs` — read once at first access,
//! cached in a process-wide `OnceLock`. Mutating env after start does not
//! retroactively change behaviour.

use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Wylde service name of the Gateway whose `egress.forward` action we call
    /// over the pipe. `WYLDE_WEBCRAWLER_GATEWAY`. Default `wylde-gateway`.
    pub gateway_service: String,

    /// The egress *caller* identity. The Gateway scopes its allowlist by the
    /// component that declared the destination key, so this MUST match the
    /// `name` in `Extensions/Webcrawler/manifest.json` (which declares the
    /// `web` egress key). `WYLDE_WEBCRAWLER_EGRESS_CALLER`. Default `Webcrawler`.
    pub egress_caller: String,

    /// The egress *destination* key declared in the manifest (wildcard
    /// `https://`). `WYLDE_WEBCRAWLER_EGRESS_DEST`. Default `web`.
    pub egress_dest: String,

    /// `User-Agent` sent on every outbound request (gateway and fallback).
    /// `WYLDE_WEBCRAWLER_USER_AGENT`. Default `Wylde-Webcrawler/1.0` — the
    /// exact string the Python handler uses.
    pub user_agent: String,
}

impl Config {
    fn load() -> Self {
        Self {
            gateway_service: env_str("WYLDE_WEBCRAWLER_GATEWAY", "wylde-gateway"),
            egress_caller: env_str("WYLDE_WEBCRAWLER_EGRESS_CALLER", "Webcrawler"),
            egress_dest: env_str("WYLDE_WEBCRAWLER_EGRESS_DEST", "web"),
            user_agent: env_str("WYLDE_WEBCRAWLER_USER_AGENT", "Wylde-Webcrawler/1.0"),
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

fn env_str(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python_handler() {
        let cfg = Config::load();
        assert_eq!(cfg.gateway_service, "wylde-gateway");
        assert_eq!(cfg.egress_caller, "Webcrawler");
        assert_eq!(cfg.egress_dest, "web");
        assert_eq!(cfg.user_agent, "Wylde-Webcrawler/1.0");
    }
}
