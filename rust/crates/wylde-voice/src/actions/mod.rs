//! Action handler modules — one per logical group.
//!
//! Slice 11.A + 11.B + 11.C + 11.D surface (each module owns the unary,
//! streaming, and listener variants of its verb):
//!
//! * [`health`] — `voice.health` (liveness + backend report).
//! * [`models`] — `voice.list_models` (HF-cache scan for Whisper / Kokoro).
//! * [`transcribe`] — `voice.transcribe` (WAV → text) +
//!   `voice.transcribe_stream` (WAV → partial transcripts as the decoder
//!   produces tokens; Slice 11.C).
//! * [`synthesize`] — `voice.synthesize` (phonemes → WAV) +
//!   `voice.synthesize_stream` (phonemes → WAV chunks per sentence
//!   batch; Slice 11.C).
//! * [`mic`] — `voice.mic.start` / `voice.mic.stop` (cpal default-input
//!   capture, singleton) + `voice.mic.chunks` (streaming PCM
//!   subscription; Slice 11.D).
//! * [`wakeword`] — `voice.wakeword.start` / `voice.wakeword.stop`
//!   (openWakeWord listener) + `voice.wakeword.events` (streaming
//!   detection subscription; Slice 11.D).
//!
//! Deferred (later slices): text→phonemes (`voice.synthesize` text path),
//! strangler-fig cutover (Slice 11.E).
//!
//! All handlers convert library-level errors into `IpcError`s via the
//! [`error`] helpers. Stable codes:
//!   * `invalid_request` — payload schema decode failed
//!   * `model_not_loaded` — model file missing on disk
//!   * `inference_failed` — `ort` raised during encoder/listener run
//!   * `audio_decode_failed` — couldn't parse WAV bytes
//!   * `npu_unavailable` — OpenVINO EP couldn't be registered at runtime
//!   * `mic_unavailable` — cpal couldn't open the default input device
//!   * `mic_busy` — mic capture singleton conflicts with the request

pub mod error;
pub mod health;
pub mod mic;
pub mod models;
pub mod session;
pub mod synthesize;
pub mod transcribe;
pub mod wakeword;

pub use error::{
    audio_decode_failed, inference_failed, invalid_request, model_not_loaded, npu_unavailable,
    require_string,
};
