//! Env-driven configuration for wylde-ext-study.
//!
//! Same shape as `wylde-ext-webcrawler`'s `config.rs` — read once at first
//! access, cached in a process-wide `OnceLock`. Mutating env after start does
//! not retroactively change behaviour.

use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Wylde service name of the harness whose S2a pipe verbs we call
    /// (`rag.add_episodic`, `rag.search`, `chat.complete`).
    /// `WYLDE_STUDY_HARNESS`. Default `wylde-harness`.
    pub harness_service: String,

    /// Default LLM model when a tool call omits `model`. Mirrors the Python
    /// handler's `_default_model()`: `WYLDE_DEFAULT_MODEL`, else `llama3`.
    /// We always pass an explicit `model` to `chat.complete` so the harness's
    /// own "model is required" guard never trips.
    pub default_model: String,
}

impl Config {
    fn load() -> Self {
        Self {
            harness_service: env_str("WYLDE_STUDY_HARNESS", "wylde-harness"),
            default_model: env_str("WYLDE_DEFAULT_MODEL", "llama3"),
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
        assert_eq!(cfg.harness_service, "wylde-harness");
        // Python `_default_model()` falls back to "llama3" when
        // WYLDE_DEFAULT_MODEL is unset — but the env var may be set in the
        // surrounding shell, so only assert the harness default here.
    }
}
