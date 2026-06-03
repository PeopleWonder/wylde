//! Env-driven configuration for wylde-harness.
//!
//! Same shape as wylde-ollama / vram-broker — read once at first access,
//! cached in a process-wide `OnceLock`. Mutating env after start does
//! not retroactively change behaviour.
//!
//! ## Env-var naming
//!
//! The consolidated crate uses the `WYLDE_HARNESS_*` prefix. The older
//! 5.A-era `WYLDE_HARNESS_TURN_*` variants are honoured as a
//! one-release fallback so a partially-rolled-out config can't
//! mis-bind a value to the default. the Wylde user's rename note (2026-05-24)
//! calls for keeping the old aliases live for a single release.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Default model when the caller doesn't pass `model`. Mirrors the
    /// Python harness' fallback (`WYLDE_DEFAULT_MODEL`); empty string
    /// means "require the caller to specify" — the chat handler will
    /// surface a clean `bad_request` if no model is resolvable.
    pub default_model: String,

    /// Per-call timeout for the upstream `ollama.chat` IPC call.
    /// Matches `WYLDE_OLLAMA_CHAT_TIMEOUT_S` semantics — 120s default.
    pub chat_timeout_s: u64,

    /// Hard ceiling on tool-call loop iterations per turn. Same as
    /// Python's `_MAX_TOOL_LOOPS = 8` in `_driver.py`. The 5.A/5.B
    /// slices run zero iterations (no tool dispatch yet); the cap
    /// lives here so 5.C can pick it up without re-deriving the value.
    pub max_tool_loops: usize,

    /// Service name to dispatch `ollama.chat` against. Lets tests
    /// retarget to a fake ollama pipe.
    pub ollama_service: String,

    /// Service name for the MCP tool host. Empty until Phase 4 lands a
    /// stable name on this box; slice 5.C will read it.
    pub extension_bridge_service: String,

    /// Service name for the tree-sitter sidecar (`wylde-treesitter`,
    /// pipe at `\\.\pipe\wylde-treesitter`). The verb-layer `code_chunk`
    /// / `code_entity` resources dispatch `treesitter.chunk` /
    /// `treesitter.extract_entities` against this over IPC — the same
    /// `call_action` hop the ollama / extension-bridge surfaces use.
    /// Lets tests retarget to a fake sidecar pipe.
    pub treesitter_service: String,

    /// Known MCP extension namespaces — the routing heuristic in
    /// [`crate::dispatch::route`] checks the dotted prefix of every
    /// tool name against this set. A real registry handshake against
    /// `wylde-extension-bridge` (`ext.list`) lands in Phase 6; for
    /// now this is a config knob. `WYLDE_HARNESS_MCP_NAMESPACES`
    /// comma-separated. Defaults to the two shipped extensions
    /// (`webcrawler,wylde_study`).
    pub mcp_namespaces: Vec<String>,

    /// Per-turn priority hint passed to `wylde-ollama` for VRAM lease
    /// scheduling. 60 mirrors the "interactive chat" tier in the
    /// broker rules.
    pub default_chat_priority: i64,

    pub wylde_root: PathBuf,
}

impl Config {
    fn load() -> Self {
        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            default_model: std::env::var("WYLDE_DEFAULT_MODEL").unwrap_or_default(),
            chat_timeout_s: env_u64_with_alias(
                "WYLDE_HARNESS_CHAT_TIMEOUT_S",
                "WYLDE_HARNESS_TURN_CHAT_TIMEOUT_S",
                120,
            ),
            max_tool_loops: env_usize_with_alias(
                "WYLDE_HARNESS_MAX_TOOL_LOOPS",
                "WYLDE_HARNESS_TURN_MAX_TOOL_LOOPS",
                8,
            ),
            ollama_service: env_string_with_alias(
                "WYLDE_HARNESS_OLLAMA_SERVICE",
                "WYLDE_HARNESS_TURN_OLLAMA_SERVICE",
                "wylde-ollama",
            ),
            extension_bridge_service: env_string_with_alias(
                "WYLDE_HARNESS_EXTENSION_BRIDGE_SERVICE",
                "WYLDE_HARNESS_TURN_EXTENSION_BRIDGE_SERVICE",
                "wylde-extension-bridge",
            ),
            treesitter_service: env_string_with_alias(
                "WYLDE_HARNESS_TREESITTER_SERVICE",
                "WYLDE_HARNESS_TURN_TREESITTER_SERVICE",
                "wylde-treesitter",
            ),
            mcp_namespaces: env_csv(
                "WYLDE_HARNESS_MCP_NAMESPACES",
                &["webcrawler", "wylde_study"],
            ),
            default_chat_priority: env_i64_with_alias(
                "WYLDE_HARNESS_CHAT_PRIORITY",
                "WYLDE_HARNESS_TURN_CHAT_PRIORITY",
                60,
            ),
            wylde_root,
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }

    /// Synthetic config for tests — does NOT touch the
    /// `OnceLock`-cached singleton. Lets tests inject overrides
    /// (e.g. `mcp_namespaces`, `extension_bridge_service`) without
    /// depending on env-var order. Exposed (not gated by `cfg(test)`)
    /// so integration tests under `crates/wylde-harness/tests/` can
    /// reach it.
    pub fn default_for_tests() -> Self {
        Self {
            default_model: String::new(),
            chat_timeout_s: 120,
            max_tool_loops: 8,
            ollama_service: "wylde-ollama".to_owned(),
            extension_bridge_service: "wylde-extension-bridge".to_owned(),
            treesitter_service: "wylde-treesitter".to_owned(),
            mcp_namespaces: vec!["webcrawler".to_owned(), "wylde_study".to_owned()],
            default_chat_priority: 60,
            wylde_root: PathBuf::from("."),
        }
    }
}

fn env_u64_with_alias(name: &str, alias: &str, default: u64) -> u64 {
    if let Ok(v) = std::env::var(name) {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    if let Ok(v) = std::env::var(alias) {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    default
}

fn env_i64_with_alias(name: &str, alias: &str, default: i64) -> i64 {
    if let Ok(v) = std::env::var(name) {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    if let Ok(v) = std::env::var(alias) {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    default
}

fn env_usize_with_alias(name: &str, alias: &str, default: usize) -> usize {
    if let Ok(v) = std::env::var(name) {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    if let Ok(v) = std::env::var(alias) {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    default
}

fn env_string_with_alias(name: &str, alias: &str, default: &str) -> String {
    std::env::var(name)
        .or_else(|_| std::env::var(alias))
        .unwrap_or_else(|_| default.to_owned())
}

fn env_csv(name: &str, default: &[&str]) -> Vec<String> {
    match std::env::var(name) {
        Ok(s) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => default.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::load();
        assert!(cfg.chat_timeout_s > 0);
        assert!(cfg.max_tool_loops > 0);
        assert_eq!(cfg.ollama_service, "wylde-ollama");
        assert_eq!(cfg.extension_bridge_service, "wylde-extension-bridge");
        assert_eq!(cfg.treesitter_service, "wylde-treesitter");
    }

    #[test]
    fn default_for_tests_carries_shipped_mcp_namespaces() {
        let cfg = Config::default_for_tests();
        assert!(cfg.mcp_namespaces.iter().any(|n| n == "webcrawler"));
        assert!(cfg.mcp_namespaces.iter().any(|n| n == "wylde_study"));
    }
}
