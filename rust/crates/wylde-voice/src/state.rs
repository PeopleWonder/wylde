//! Process-wide service state — model handles + lease bookkeeping.
//!
//! The Python predecessor's [`Voice/state.py`](../../../../Voice/state.py)
//! tracked an orchestrator-style state machine (push-to-talk mode,
//! active conversation, in-flight session). The Rust port doesn't carry
//! orchestration state — the harness composes the primitives — so this
//! module is much smaller: it caches loaded inference handles and the
//! VRAM lease that backs each one so the next call to the same action
//! reuses the warm session instead of paying the load cost.
//!
//! Slice 11.A+ slots:
//!   * `whisper_encoder` (+ paired NPU/CPU encoder lease)
//!   * `whisper_decoder` (+ paired CPU-DRAM decoder lease)
//!   * `whisper_tokenizer`
//!
//! Slice 11.B adds:
//!   * `kokoro_synth` (+ paired DRAM lease — Kokoro is CPU-only, see
//!     [`crate::synth::kokoro`] for why)
//!   * `kokoro_voices` (no lease — ~14 MB heap, under broker threshold)
//!
//! Slice 11.D adds:
//!   * `mic_capture` — `cpal`-backed default-input handle. Singleton;
//!     held as `Arc` so both the `voice.mic.chunks` streaming handler
//!     and the wake-word listener can fan-out the same broadcast.
//!   * `wakeword_pipeline` — loaded openWakeWord ONNX bundle; cached
//!     here so a stop/start cycle skips the 3× ONNX load cost.
//!   * `wakeword_listener` — running listener thread + event broadcast.

use std::sync::{Arc, OnceLock, RwLock};

use crate::lease::Lease;
use crate::mic::MicCapture;
use crate::synth::{KokoroSynth, Voices};
use crate::transcribe::{WhisperDecoder, WhisperEncoder, WhisperTokenizer};
use crate::wakeword::{WakeWordListener, WakeWordPipeline};

/// Snapshot of loaded inference handles. Cheap to read (RwLock read
/// guard, no model allocation copy); construction holds the write
/// guard for the duration of the model load.
pub struct State {
    whisper_encoder: Option<Arc<WhisperEncoder>>,
    whisper_encoder_lease: Option<Lease>,

    whisper_decoder: Option<Arc<WhisperDecoder>>,
    whisper_decoder_lease: Option<Lease>,

    whisper_tokenizer: Option<Arc<WhisperTokenizer>>,

    kokoro_synth: Option<Arc<KokoroSynth>>,
    kokoro_synth_lease: Option<Lease>,

    kokoro_voices: Option<Arc<Voices>>,

    mic_capture: Option<Arc<MicCapture>>,
    wakeword_pipeline: Option<Arc<WakeWordPipeline>>,
    wakeword_listener: Option<Arc<WakeWordListener>>,
}

impl State {
    fn new() -> Self {
        Self {
            whisper_encoder: None,
            whisper_encoder_lease: None,
            whisper_decoder: None,
            whisper_decoder_lease: None,
            whisper_tokenizer: None,
            kokoro_synth: None,
            kokoro_synth_lease: None,
            kokoro_voices: None,
            mic_capture: None,
            wakeword_pipeline: None,
            wakeword_listener: None,
        }
    }
}

fn state() -> &'static RwLock<State> {
    static S: OnceLock<RwLock<State>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(State::new()))
}

pub fn whisper_encoder() -> Option<Arc<WhisperEncoder>> {
    state().read().ok().and_then(|s| s.whisper_encoder.clone())
}

pub fn set_whisper_encoder(handle: Arc<WhisperEncoder>, lease: Option<Lease>) {
    if let Ok(mut s) = state().write() {
        s.whisper_encoder = Some(handle);
        s.whisper_encoder_lease = lease;
    }
}

pub fn whisper_decoder() -> Option<Arc<WhisperDecoder>> {
    state().read().ok().and_then(|s| s.whisper_decoder.clone())
}

pub fn set_whisper_decoder(handle: Arc<WhisperDecoder>, lease: Option<Lease>) {
    if let Ok(mut s) = state().write() {
        s.whisper_decoder = Some(handle);
        s.whisper_decoder_lease = lease;
    }
}

pub fn whisper_tokenizer() -> Option<Arc<WhisperTokenizer>> {
    state()
        .read()
        .ok()
        .and_then(|s| s.whisper_tokenizer.clone())
}

pub fn set_whisper_tokenizer(handle: Arc<WhisperTokenizer>) {
    if let Ok(mut s) = state().write() {
        s.whisper_tokenizer = Some(handle);
    }
}

pub fn kokoro_synth() -> Option<Arc<KokoroSynth>> {
    state().read().ok().and_then(|s| s.kokoro_synth.clone())
}

pub fn set_kokoro_synth(handle: Arc<KokoroSynth>, lease: Option<Lease>) {
    if let Ok(mut s) = state().write() {
        s.kokoro_synth = Some(handle);
        s.kokoro_synth_lease = lease;
    }
}

pub fn kokoro_voices() -> Option<Arc<Voices>> {
    state().read().ok().and_then(|s| s.kokoro_voices.clone())
}

pub fn set_kokoro_voices(handle: Arc<Voices>) {
    if let Ok(mut s) = state().write() {
        s.kokoro_voices = Some(handle);
    }
}

/// Drop encoder + decoder + tokenizer + their leases. Called from
/// [`crate::service::stop`] on shutdown and by tests via
/// [`reset_for_tests`].
pub fn clear_whisper_encoder() {
    if let Ok(mut s) = state().write() {
        s.whisper_encoder = None;
        s.whisper_encoder_lease = None;
        s.whisper_decoder = None;
        s.whisper_decoder_lease = None;
        s.whisper_tokenizer = None;
    }
}

/// Drop the Kokoro session + its lease and the voices bundle. Called
/// from [`crate::service::stop`] on shutdown and by tests via
/// [`reset_for_tests`].
pub fn clear_kokoro() {
    if let Ok(mut s) = state().write() {
        s.kokoro_synth = None;
        s.kokoro_synth_lease = None;
        s.kokoro_voices = None;
    }
}

// ── Slice 11.D: mic + wake-word slots ───────────────────────────────

pub fn mic_capture() -> Option<Arc<MicCapture>> {
    state().read().ok().and_then(|s| s.mic_capture.clone())
}

pub fn set_mic_capture(handle: Arc<MicCapture>) {
    if let Ok(mut s) = state().write() {
        s.mic_capture = Some(handle);
    }
}

/// Take ownership of the mic capture slot. Returns the previous handle
/// if there was one. Used by `voice.mic.stop` so the cpal stream is
/// dropped synchronously (last `Arc` reference dies → `MicCapture::Drop`
/// joins the worker thread).
pub fn take_mic_capture() -> Option<Arc<MicCapture>> {
    state().write().ok().and_then(|mut s| s.mic_capture.take())
}

pub fn wakeword_pipeline() -> Option<Arc<WakeWordPipeline>> {
    state()
        .read()
        .ok()
        .and_then(|s| s.wakeword_pipeline.clone())
}

pub fn set_wakeword_pipeline(handle: Arc<WakeWordPipeline>) {
    if let Ok(mut s) = state().write() {
        s.wakeword_pipeline = Some(handle);
    }
}

pub fn wakeword_listener() -> Option<Arc<WakeWordListener>> {
    state()
        .read()
        .ok()
        .and_then(|s| s.wakeword_listener.clone())
}

pub fn set_wakeword_listener(handle: Arc<WakeWordListener>) {
    if let Ok(mut s) = state().write() {
        s.wakeword_listener = Some(handle);
    }
}

pub fn take_wakeword_listener() -> Option<Arc<WakeWordListener>> {
    state()
        .write()
        .ok()
        .and_then(|mut s| s.wakeword_listener.take())
}

/// Drop the wake-word listener + pipeline + mic. Called from
/// [`crate::service::stop`] on shutdown.
pub fn clear_voice_io() {
    if let Ok(mut s) = state().write() {
        s.wakeword_listener = None;
        s.wakeword_pipeline = None;
        s.mic_capture = None;
    }
}

/// Test-only: reset every slot. Called from
/// [`crate::service::reset_for_tests`].
pub fn reset_for_tests() {
    clear_whisper_encoder();
    clear_kokoro();
    clear_voice_io();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_state_has_no_handles() {
        reset_for_tests();
        assert!(whisper_encoder().is_none());
        assert!(whisper_decoder().is_none());
        assert!(whisper_tokenizer().is_none());
        assert!(kokoro_synth().is_none());
        assert!(kokoro_voices().is_none());
    }
}
