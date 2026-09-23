//! Env-driven configuration for `wylde-voice`.
//!
//! Same shape as the broker / ollama / trainer `config.rs` — read once
//! at first access, cached in a process-wide `OnceLock`. The Python
//! Voice service reads `Voice/config.yaml`; the Rust port intentionally
//! drops the YAML layer in favour of env-var-only configuration to
//! match the rest of the Rust ring. Migration: any operator that was
//! editing the YAML can flip the equivalent env vars (see comments).
//!
//! The `WYLDE_VOICE_*` env-var names mirror the Python implementation
//! where they overlap so a single export covers both during the
//! strangler-fig phase.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Whisper backend selection. The spike confirmed both work; the
/// surprising finding was that CPU is *faster* than NPU on the Wylde user's
/// machine at whisper-tiny scale — flip to NPU at whisper-small+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttBackend {
    /// Stock ORT CPU EP. The default — universally available, no DLL
    /// dance required beyond the bundled onnxruntime.dll.
    Cpu,
    /// OpenVINO Execution Provider targeting Intel NPU (with HETERO
    /// CPU fallback for the dynamic-shape decoder). Requires the
    /// matched-version OpenVINO + ORT DLL bundle on the binary's
    /// search path (spike findings, "Build configuration that works").
    Npu,
}

impl SttBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            SttBackend::Cpu => "cpu",
            SttBackend::Npu => "npu",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "npu" => SttBackend::Npu,
            _ => SttBackend::Cpu,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// `WYLDE_ROOT`. Cached so `data/contracts/actions/wylde-voice.json`
    /// resolves to the same tree every other Rust service writes into.
    pub wylde_root: PathBuf,

    /// Whisper backend — `WYLDE_VOICE_WHISPER_BACKEND` (mirrors the
    /// Python env var name). Default: CPU.
    pub stt_backend: SttBackend,

    /// HF repo id for the Whisper model. `WYLDE_VOICE_STT_MODEL`.
    /// Default: `openai/whisper-small` (same as Python's
    /// `Voice/config.yaml`).
    pub stt_model: String,

    /// Override the encoder ONNX path directly. Lets a developer point
    /// the service at a hand-exported model without going through the
    /// HF cache resolver. `WYLDE_VOICE_STT_ENCODER_PATH`.
    pub stt_encoder_path_override: Option<PathBuf>,

    /// OpenVINO device hint when `stt_backend == Npu`.
    /// `WYLDE_VOICE_OV_DEVICE`. Default `"NPU"`. Accepts the same
    /// tokens the spike used: `NPU`, `CPU`, `GPU`, `HETERO:NPU,CPU`.
    pub ov_device: String,

    /// Where the NPU cache lives. Per spike findings (§"What the spike
    /// did NOT prove" → "cache_dir is mandatory for NPU"): without it,
    /// each cold start eats ~2.5s on encoder compile.
    /// `WYLDE_VOICE_OV_CACHE_DIR`. Default:
    /// `%LOCALAPPDATA%\Wylde\voice\ov_cache` on Windows, else
    /// `<WYLDE_ROOT>/cache/voice/ov_cache`.
    pub ov_cache_dir: PathBuf,

    /// TTS voice catalogue default. `WYLDE_VOICE_TTS_VOICE`. Default
    /// `af_heart` (mirrors `Voice/config.yaml`).
    pub tts_voice: String,

    /// TTS playback speed multiplier. `WYLDE_VOICE_TTS_SPEED`. Default 1.0.
    pub tts_speed: f32,

    // ── VRAM lease (Slice 11.A+: only used when transcribe acquires
    //    a real model into memory; today's encoder-only proof still
    //    asks for a lease so the broker accounts for the NPU buffer
    //    footprint).
    /// Service name for the broker we lease against. Lets tests retarget
    /// to a fake broker pipe. `WYLDE_VOICE_BROKER_SERVICE`.
    pub broker_service: String,

    /// Default priority tier for voice lease requests. 50 = interactive
    /// (the default Python harness uses; same number `wylde-ollama` uses
    /// for chat-stream callers, see lease.rs:43).
    pub default_priority: i64,

    /// Lease TTL in seconds. Same default as wylde-ollama (60s) so the
    /// broker's housekeeping cadence stays uniform.
    pub lease_ttl_s: f64,

    /// Heartbeat cadence — TTL / 3, conventional.
    pub lease_heartbeat_s: u64,

    /// Per-call timeout for `voice.health`. `WYLDE_VOICE_HEALTH_TIMEOUT_S`.
    pub health_timeout_s: u64,

    /// Per-call timeout for `voice.transcribe`. Default 30 s — well over
    /// the spike's 143 ms median, gives plenty of head-room for first-
    /// load NPU compile (~2.5s) on a cold cache.
    /// `WYLDE_VOICE_TRANSCRIBE_TIMEOUT_S`.
    pub transcribe_timeout_s: u64,

    /// Default openWakeWord model name (Slice 11.D). Mirrors Python's
    /// `Voice/state.py::DEFAULT_WAKE_WORD_MODEL`. The matching ONNX
    /// bundle is resolved at `<wakeword_models_dir>/<model_name>/`.
    /// `WYLDE_VOICE_WAKEWORD_MODEL`.
    pub wakeword_model: String,

    /// Where openWakeWord bundles live on disk. Default:
    /// `<WYLDE_ROOT>/cache/voice/wakeword`. First-run model setup
    /// drops `<model_name>/{melspectrogram,embedding_model,<model_name>}.onnx`
    /// here. `WYLDE_VOICE_WAKEWORD_MODELS_DIR`.
    pub wakeword_models_dir: PathBuf,

    // ── VAD (Slice 3 — silence-triggered capture) ───────────────────────
    /// Speech-probability threshold for the energy+ZCR VAD. Mirrors
    /// Python's `vad.threshold` (`Voice/config.yaml`); the YAML layer is
    /// dropped in the Rust port, so it's read from
    /// `WYLDE_VOICE_VAD_THRESHOLD`. Default 0.65.
    pub vad_threshold: f32,

    /// Trailing silence (ms) after speech starts that ends an utterance.
    /// Mirrors Python's `vad.silence_timeout_ms`. Read from
    /// `WYLDE_VOICE_VAD_SILENCE_TIMEOUT_MS`. Default 1800.
    pub vad_silence_timeout_ms: u32,
}

impl Config {
    /// Project the env-driven VAD knobs onto the detector's own config
    /// struct, used by the capture adapter's silence-triggered loop.
    pub fn vad_config(&self) -> crate::vad::VadConfig {
        crate::vad::VadConfig {
            threshold: self.vad_threshold,
            silence_timeout_ms: self.vad_silence_timeout_ms,
        }
    }
}

impl Config {
    fn load() -> Self {
        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let stt_backend = SttBackend::parse(
            &std::env::var("WYLDE_VOICE_WHISPER_BACKEND").unwrap_or_else(|_| "cpu".to_owned()),
        );

        let stt_model = std::env::var("WYLDE_VOICE_STT_MODEL")
            .unwrap_or_else(|_| "openai/whisper-small".to_owned());

        let stt_encoder_path_override =
            std::env::var_os("WYLDE_VOICE_STT_ENCODER_PATH").map(PathBuf::from);

        let ov_device = std::env::var("WYLDE_VOICE_OV_DEVICE").unwrap_or_else(|_| "NPU".to_owned());

        let ov_cache_dir = std::env::var_os("WYLDE_VOICE_OV_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_ov_cache_dir(&wylde_root));

        let tts_voice =
            std::env::var("WYLDE_VOICE_TTS_VOICE").unwrap_or_else(|_| "af_heart".to_owned());

        let tts_speed: f32 = std::env::var("WYLDE_VOICE_TTS_SPEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);

        let wakeword_models_dir = std::env::var_os("WYLDE_VOICE_WAKEWORD_MODELS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_wakeword_models_dir(&wylde_root));

        Self {
            wylde_root,
            stt_backend,
            stt_model,
            stt_encoder_path_override,
            ov_device,
            ov_cache_dir,
            tts_voice,
            tts_speed,
            broker_service: std::env::var("WYLDE_VOICE_BROKER_SERVICE")
                .unwrap_or_else(|_| "wylde-vram-broker".to_owned()),
            default_priority: env_i64("WYLDE_VOICE_PRIORITY", 50),
            lease_ttl_s: env_f64("WYLDE_VOICE_LEASE_TTL_S", 60.0),
            lease_heartbeat_s: env_u64("WYLDE_VOICE_LEASE_HEARTBEAT_S", 20),
            health_timeout_s: env_u64("WYLDE_VOICE_HEALTH_TIMEOUT_S", 3),
            transcribe_timeout_s: env_u64("WYLDE_VOICE_TRANSCRIBE_TIMEOUT_S", 30),
            wakeword_model: std::env::var("WYLDE_VOICE_WAKEWORD_MODEL")
                .unwrap_or_else(|_| "openWakeWord/hey-jarvis".to_owned()),
            wakeword_models_dir,
            vad_threshold: env_f32("WYLDE_VOICE_VAD_THRESHOLD", crate::vad::DEFAULT_THRESHOLD),
            vad_silence_timeout_ms: env_u32(
                "WYLDE_VOICE_VAD_SILENCE_TIMEOUT_MS",
                crate::vad::DEFAULT_SILENCE_TIMEOUT_MS,
            ),
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

fn default_ov_cache_dir(wylde_root: &std::path::Path) -> PathBuf {
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local)
                .join("Wylde")
                .join("voice")
                .join("ov_cache");
        }
    }
    wylde_root.join("cache").join("voice").join("ov_cache")
}

fn default_wakeword_models_dir(wylde_root: &std::path::Path) -> PathBuf {
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local)
                .join("Wylde")
                .join("voice")
                .join("wakeword");
        }
    }
    wylde_root.join("cache").join("voice").join("wakeword")
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

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
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
        assert!(!cfg.stt_model.is_empty());
        assert!(!cfg.ov_device.is_empty());
        assert!(!cfg.tts_voice.is_empty());
        assert!(cfg.tts_speed > 0.0);
        assert!(cfg.lease_ttl_s > 0.0);
        assert!(cfg.health_timeout_s > 0);
        assert!(cfg.transcribe_timeout_s >= cfg.health_timeout_s);
        // Default backend is CPU per spike-finding "for Whisper-tiny on
        // this Arrow Lake NPU, the CPU is faster".
        assert_eq!(cfg.stt_backend, SttBackend::Cpu);
        // Slice 11.D — wake-word default model + models dir.
        assert!(!cfg.wakeword_model.is_empty());
        assert!(cfg.wakeword_models_dir.components().count() > 0);
        // Slice 3 — VAD defaults mirror Python's VadConfig.
        assert_eq!(cfg.vad_threshold, 0.65);
        assert_eq!(cfg.vad_silence_timeout_ms, 1_800);
        assert_eq!(cfg.vad_config().threshold, cfg.vad_threshold);
    }

    #[test]
    fn stt_backend_parses_npu() {
        assert_eq!(SttBackend::parse("npu"), SttBackend::Npu);
        assert_eq!(SttBackend::parse("NPU"), SttBackend::Npu);
        assert_eq!(SttBackend::parse(" cpu "), SttBackend::Cpu);
        // Unknown → CPU (safe default).
        assert_eq!(SttBackend::parse("tpu"), SttBackend::Cpu);
    }

    #[test]
    fn stt_backend_as_str_roundtrips() {
        assert_eq!(SttBackend::Cpu.as_str(), "cpu");
        assert_eq!(SttBackend::Npu.as_str(), "npu");
    }
}
