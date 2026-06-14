//! Env-driven configuration for `wylde-lsp`. Read once, cached in a
//! process-wide `OnceLock` — same shape as every other service's `config.rs`.

use std::path::PathBuf;
use std::sync::OnceLock;

pub const DEFAULT_SERVICE_NAME: &str = "wylde-lsp";

#[derive(Debug, Clone)]
pub struct Config {
    /// Pipe/service name. Override via `WYLDE_LSP_PIPE_NAME` (tests bind an
    /// isolated pipe).
    pub service_name: String,
    /// `rust-analyzer` binary. Override via `WYLDE_LSP_RA_PATH`; defaults to
    /// `rust-analyzer` (resolved on `PATH`). When it can't be spawned the
    /// service stays up and reports unavailable.
    pub rust_analyzer: String,
    /// Repo/install root (manifest contract path base).
    pub wylde_root: PathBuf,
    /// Per-request timeout (ms) for completion/hover round-trips. Override via
    /// `WYLDE_LSP_REQUEST_TIMEOUT_MS`.
    pub request_timeout_ms: u64,
}

impl Config {
    fn load() -> Self {
        let service_name = std::env::var("WYLDE_LSP_PIPE_NAME")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());

        let rust_analyzer = std::env::var("WYLDE_LSP_RA_PATH")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "rust-analyzer".to_owned());

        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let request_timeout_ms = std::env::var("WYLDE_LSP_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(5_000);

        Self {
            service_name,
            rust_analyzer,
            wylde_root,
            request_timeout_ms,
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::load();
        assert!(!cfg.service_name.is_empty());
        assert!(!cfg.rust_analyzer.is_empty());
        assert!(cfg.request_timeout_ms > 0);
    }
}
