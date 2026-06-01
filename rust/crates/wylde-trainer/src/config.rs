//! Env-driven configuration for wylde-trainer.
//!
//! Same shape as the broker / ollama `config.rs` — read once at first
//! access, cached in a process-wide `OnceLock`. The trainer is a thin
//! pipe-to-pipe forwarder; the heavy inference knobs (model variant,
//! detail level) are read by the Python worker from its own
//! `Trainer/Caption/config.py`. We only carry the per-action call
//! timeouts and a copy of the worker's announced defaults for the
//! `caption.list_backends` / health reporting surface.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Caption backend the worker is expected to boot with: `florence`,
    /// `qwen`, or `joycaption`. Surfaced from the manifest +
    /// `caption.list_backends`. `CAPTION_BACKEND`.
    pub backend: String,

    /// Florence-2 variant. Surfaced from the manifest. `CAPTION_FLORENCE_VARIANT`.
    pub florence_variant: String,

    /// Default detail level. Surfaced from the manifest. `CAPTION_DETAIL`.
    pub default_detail: String,

    /// Per-call timeout for `caption.generate` (single image).
    /// `WYLDE_TRAINER_GENERATE_TIMEOUT_S`. Default 60 s — the first call
    /// loads ~1.5 GB of Florence-2 weights and can take 20-30 s; later
    /// calls return in single-digit seconds.
    pub generate_timeout_s: u64,

    /// Per-call timeout for `caption.generate_batch` (folder walk).
    /// `WYLDE_TRAINER_BATCH_TIMEOUT_S`. Default 3600 s (one hour); the
    /// caller typically sets the bound based on folder size.
    pub batch_timeout_s: u64,

    /// Per-call timeout for `caption.generate_video`. Default 1800 s.
    /// `WYLDE_TRAINER_VIDEO_TIMEOUT_S`.
    pub video_timeout_s: u64,

    /// Per-call timeout for `caption.health` — fast probe.
    /// `WYLDE_TRAINER_HEALTH_TIMEOUT_S`. Default 5 s.
    pub health_timeout_s: u64,

    pub wylde_root: PathBuf,
}

impl Config {
    fn load() -> Self {
        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            backend: std::env::var("CAPTION_BACKEND")
                .unwrap_or_else(|_| "florence".to_owned())
                .to_lowercase(),
            florence_variant: std::env::var("CAPTION_FLORENCE_VARIANT")
                .unwrap_or_else(|_| "large".to_owned())
                .to_lowercase(),
            default_detail: std::env::var("CAPTION_DETAIL")
                .unwrap_or_else(|_| "detailed".to_owned())
                .to_lowercase(),
            generate_timeout_s: env_u64("WYLDE_TRAINER_GENERATE_TIMEOUT_S", 60),
            batch_timeout_s: env_u64("WYLDE_TRAINER_BATCH_TIMEOUT_S", 3600),
            video_timeout_s: env_u64("WYLDE_TRAINER_VIDEO_TIMEOUT_S", 1800),
            health_timeout_s: env_u64("WYLDE_TRAINER_HEALTH_TIMEOUT_S", 5),
            wylde_root,
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
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
        assert!(!cfg.backend.is_empty());
        assert!(!cfg.florence_variant.is_empty());
        assert!(cfg.generate_timeout_s > 0);
        assert!(cfg.batch_timeout_s >= cfg.generate_timeout_s);
        assert!(cfg.health_timeout_s > 0);
    }

    #[test]
    fn backend_normalised_to_lowercase() {
        std::env::set_var("CAPTION_BACKEND", "Florence");
        let cfg = Config::load();
        assert_eq!(cfg.backend, "florence");
        std::env::remove_var("CAPTION_BACKEND");
    }
}
