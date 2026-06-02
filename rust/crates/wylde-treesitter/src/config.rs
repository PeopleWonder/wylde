//! Env-driven configuration for wylde-treesitter.
//!
//! Same shape as `wylde-ollama`'s `config.rs` — read once at first access,
//! cached in a process-wide `OnceLock`. Mutating env after start does not
//! retroactively change behaviour.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Repo root. `WYLDE_ROOT`. Used to place the action contract file at
    /// `data/contracts/actions/wylde-treesitter.json`. Defaults to `.`.
    pub wylde_root: PathBuf,

    /// Hard ceiling on inline `source` bytes accepted by `treesitter.parse`.
    /// Past this the verb rejects with a structured `invalid_request` rather
    /// than handing a pathological (e.g. minified) file to the parser, which
    /// can balloon AST node counts (plan risk #4). `WYLDE_TREESITTER_MAX_SOURCE_BYTES`.
    /// Default 2 MiB — comfortably under the 64 MB pipe frame cap.
    pub max_source_bytes: usize,

    /// Maximum AST depth serialised by `treesitter.parse`. Nodes deeper than
    /// this are elided (their `children` array is omitted and `truncated:true`
    /// is set) so a deeply-nested file can't produce an unbounded reply.
    /// `WYLDE_TREESITTER_MAX_PARSE_DEPTH`. Default 64.
    pub max_parse_depth: usize,
}

impl Config {
    fn load() -> Self {
        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            wylde_root,
            max_source_bytes: env_usize("WYLDE_TREESITTER_MAX_SOURCE_BYTES", 2 * 1024 * 1024),
            max_parse_depth: env_usize("WYLDE_TREESITTER_MAX_PARSE_DEPTH", 64),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::load();
        assert!(cfg.max_source_bytes > 0);
        assert!(cfg.max_parse_depth > 0);
        // Inline source must stay well under the 64 MB pipe frame cap.
        assert!(cfg.max_source_bytes < 64 * 1024 * 1024);
    }
}
