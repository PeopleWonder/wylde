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

    /// Default ceiling on a single chunk's byte length for `treesitter.chunk`
    /// when the caller doesn't pass `max_chunk_bytes`. AST boundaries are kept
    /// whole up to this size; a definition larger than this is sub-split into
    /// line-aligned byte windows so a giant function/class can't produce one
    /// embedding-busting chunk. `WYLDE_TREESITTER_MAX_CHUNK_BYTES`. Default
    /// 24 KiB — large enough to keep ordinary functions/classes intact, small
    /// enough that a pathological definition still gets windowed.
    pub max_chunk_bytes: usize,

    /// Localhost TCP port for the HTTP front door (`http.rs`). N8N's HTTP
    /// Request node can't open a Windows named pipe, so the sidecar also
    /// serves the chunk surface over `127.0.0.1:<port>` (same belt-and-
    /// suspenders shape `memgraph.py` uses: pipe canonical, HTTP for N8N).
    /// Bound to loopback only. `WYLDE_TREESITTER_HTTP_PORT`. Default 8030.
    pub http_port: u16,
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
            max_chunk_bytes: env_usize("WYLDE_TREESITTER_MAX_CHUNK_BYTES", 24 * 1024),
            http_port: env_u16("WYLDE_TREESITTER_HTTP_PORT", 8030),
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

fn env_u16(name: &str, default: u16) -> u16 {
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
        // A chunk ceiling must leave room for an ordinary definition but stay
        // under the whole-file source ceiling.
        assert!(cfg.max_chunk_bytes > 0);
        assert!(cfg.max_chunk_bytes <= cfg.max_source_bytes);
        // HTTP front door binds a real port in the Wylde 8000–8999 range.
        assert!(cfg.http_port >= 8000);
    }
}
