//! Production adapters for the [`crate::orchestrator`] traits
//! (Slice 11.E+).
//!
//! The orchestrator's trait surface (`AudioCapture`, `AudioPlayback`,
//! `HarnessChat`) was designed in Slice 11.E with mocks for unit tests
//! but no production wiring. This module hosts those wirings so the
//! GUI-facing `voice.toggle` handler can drive a real session.
//!
//! ## How the pieces fit
//!
//! * [`MicSessionCapture`] — Adapts the singleton [`crate::mic::MicCapture`]
//!   into a "record one utterance, return the buffer" shape. Subscribes
//!   to the broadcast for the call, collects into a Vec<i16>, returns.
//!   A process-wide cancel signal lets `voice.end_session` early-terminate.
//!   Slice 3: in always-on mode it runs the energy+ZCR [`crate::vad`] gate
//!   so capture stops on silence (parity with Python's
//!   `record_until_silence`); push-to-talk keeps the hold-until-cancel /
//!   fixed-cap shape. `max_seconds` is the hard cap for both.
//! * [`CpalPlayback`] — Thin wrapper over [`crate::playback::play_blocking`].
//! * [`HarnessIpcClient`] — Routes the chat turn (`chat.run_turn`) over the
//!   shared IPC primitive. STT and TTS are both handled in-process: TTS
//!   since Slice 2 (`synthesize` calls this crate's own `voice.synthesize`
//!   handler — Rust G2P (`synth::g2p`) + local Kokoro), and STT since the
//!   Slice 11.E cutover (`transcribe` calls this crate's own
//!   `voice.transcribe` handler — ONNX Whisper owned by
//!   `crate::actions::transcribe`). Neither round-trips to the retired
//!   Python harness `models.transcribe` / `models.synthesize` verbs.
//!   Conversation resolution falls back to `conversations.list` and picks
//!   the most recent entry — mirrors the prior `Voice/orchestrator.py`
//!   `_resolve_conversation`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use wylde_shared::ipc::send_action;

use crate::config::Config;
use crate::config_persist::MODE_ALWAYS_ON;
use crate::mic::{MicCapture, DEFAULT_MIC_CHUNK_SAMPLES, TARGET_SAMPLE_RATE};
use crate::orchestrator::{
    AudioCapture, AudioPlayback, AudioUnavailable, ChatTurnResult, HarnessCallError, HarnessChat,
    SynthResult,
};
use crate::playback::{play_blocking, PlaybackError};
use crate::service_state::ServiceState;
use crate::state;
use crate::vad::{GateDecision, VadGate};

/// Service-name target for the harness calls. Matches the existing
/// `\\.\pipe\wylde-harness` pipe.
pub const HARNESS_SERVICE: &str = "wylde-harness";

/// Production capture adapter — drives [`crate::mic::MicCapture`] for
/// one round-trip. Maintains a process-wide "abort" flag so the GUI's
/// `voice.end_session` action can return early without tearing down the
/// underlying cpal stream.
pub struct MicSessionCapture {
    cancel: Arc<AtomicBool>,
}

impl MicSessionCapture {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a clone of the abort flag — the `voice.end_session` handler
    /// holds onto this so it can signal stop without re-resolving the
    /// singleton.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }
}

impl Default for MicSessionCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AudioCapture for MicSessionCapture {
    async fn capture(&self, max_seconds: f32) -> Result<Vec<i16>, AudioUnavailable> {
        self.cancel.store(false, Ordering::SeqCst);

        // Singleton-or-new: reuse an in-progress mic capture when one
        // exists (avoids fighting wakeword.start for the device). For
        // session-driven capture we don't pin the chunk size — fall back
        // to the default 50 ms frame.
        let capture: Arc<MicCapture> = match state::mic_capture() {
            Some(existing) => existing,
            None => {
                let c = MicCapture::start(DEFAULT_MIC_CHUNK_SAMPLES)
                    .map_err(|e| AudioUnavailable(format!("mic start: {e}")))?;
                let arc = Arc::new(c);
                state::set_mic_capture(Arc::clone(&arc));
                arc
            }
        };

        let mut rx = capture.subscribe();
        let deadline = Instant::now() + Duration::from_secs_f32(max_seconds.max(0.0));

        // Slice 3: in always-on mode the capture ends on silence (VAD-
        // gated, matching Python's `record_until_silence`); in push-to-talk
        // mode the user holds the button, so capture runs until the cancel
        // flag fires (button release / `voice.end_session`). `max_seconds`
        // is the hard cap / fallback for both — a VAD that never sees the
        // user stop, or a wedged button, still terminates at the deadline.
        let vad_gated = ServiceState::global().get_mode().await == MODE_ALWAYS_ON;

        if vad_gated {
            let mut gate = VadGate::new(&Config::get().vad_config(), TARGET_SAMPLE_RATE);
            loop {
                if self.cancel.load(Ordering::SeqCst) {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Ok(chunk)) => {
                        if gate.observe(&chunk) == GateDecision::SpeechEnded {
                            break;
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                    Err(_) => break, // deadline elapsed inside recv
                }
            }
            if let Some((start_ms, end_ms)) = gate.speech_span_ms() {
                tracing::debug!(
                    "wylde-voice: VAD captured speech span {start_ms}..{end_ms} ms \
                     ({} samples)",
                    gate.speech_len(),
                );
            }
            return Ok(gate.into_speech());
        }

        // Push-to-talk: fixed-duration / hold capture (unchanged shape).
        let mut buffer: Vec<i16> = Vec::new();
        let target_samples = (max_seconds.max(0.0) * TARGET_SAMPLE_RATE as f32) as usize;
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            if buffer.len() >= target_samples {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_) => break, // deadline elapsed inside recv
            }
        }

        Ok(buffer)
    }

    fn sample_rate(&self) -> u32 {
        TARGET_SAMPLE_RATE
    }
}

/// Production playback adapter. cpal-only; one fresh stream per call.
pub struct CpalPlayback;

#[async_trait]
impl AudioPlayback for CpalPlayback {
    async fn play(&self, pcm_i16: Vec<i16>, sample_rate: u32) -> Result<(), PlaybackError> {
        play_blocking(pcm_i16, sample_rate).await
    }
}

/// Production harness adapter — routes the three harness calls through
/// the shared IPC client.
pub struct HarnessIpcClient {
    /// Service name we point the harness calls at. Overridable so a
    /// test can target a fake pipe.
    pub service: String,
}

impl HarnessIpcClient {
    pub fn new() -> Self {
        Self {
            service: HARNESS_SERVICE.to_owned(),
        }
    }
}

impl Default for HarnessIpcClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HarnessChat for HarnessIpcClient {
    async fn transcribe(
        &self,
        audio: &[i16],
        sample_rate: u32,
    ) -> Result<String, HarnessCallError> {
        // Slice 11.E cutover: STT is now Rust-native and in-process. The
        // orchestrator used to round-trip raw PCM to the Python harness
        // `models.transcribe` (faster-whisper inside `Voice/`); that
        // engine was retired with the Python `Voice/` tree. We now call
        // this crate's own `voice.transcribe` handler (ONNX Whisper —
        // `crate::actions::transcribe`), mirroring how Slice 2 moved TTS
        // in-process.
        //
        // Empty capture (e.g. a VAD-gated window that saw no speech) →
        // empty transcript, so the orchestrator's `empty_transcript`
        // short-circuit fires instead of an audio-decode error. This
        // preserves the behaviour of the old Python path, which decoded
        // an empty buffer to "".
        if audio.is_empty() {
            return Ok(String::new());
        }
        // `handle_transcribe` consumes a self-describing WAV (it resamples
        // and peak-normalises internally), so wrap the captured PCM into a
        // 16-bit mono WAV first.
        let wav = pcm_i16_to_wav(audio, sample_rate)
            .map_err(|e| HarnessCallError(format!("voice.transcribe: WAV encode: {e}")))?;
        let b64 = BASE64.encode(&wav);
        let reply =
            crate::actions::transcribe::handle_transcribe(json!({ "audio_b64": b64 })).await;
        if !reply.ok {
            return Err(HarnessCallError(format_reply_error(
                &reply,
                "voice.transcribe",
            )));
        }
        Ok(reply
            .data
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn run_chat_turn(
        &self,
        user_message: &str,
        conversation_id: &str,
        model: Option<&str>,
    ) -> Result<ChatTurnResult, HarnessCallError> {
        let payload = json!({
            "user_message": user_message,
            "conversation_id": conversation_id,
            "model": model,
            "modality": "voice",
        });
        let reply = send_action(&self.service, "chat.run_turn", payload).await;
        if !reply.ok {
            return Err(HarnessCallError(format_reply_error(
                &reply,
                "chat.run_turn",
            )));
        }
        let final_message = reply
            .data
            .get("final_message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let aborted = reply
            .data
            .get("aborted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let abort_reason = reply
            .data
            .get("abort_reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(ChatTurnResult {
            final_message,
            aborted,
            abort_reason,
        })
    }

    async fn synthesize(&self, text: &str) -> Result<SynthResult, HarnessCallError> {
        // Slice 2: TTS is now Rust-native. The text→phoneme step (G2P)
        // lives in `crate::synth::g2p` (misaki-rs) and Kokoro inference is
        // owned by this crate, so the orchestrator no longer round-trips
        // to the Python harness `models.synthesize` (which phonemised via
        // espeak-ng). We call the in-process `voice.synthesize` handler
        // directly — it returns a base64 WAV in `audio`, which the
        // orchestrator's `decode_wav_to_i16` consumes.
        let reply = crate::actions::synthesize::handle_synthesize(json!({"text": text})).await;
        if !reply.ok {
            return Err(HarnessCallError(format_reply_error(
                &reply,
                "voice.synthesize",
            )));
        }
        let audio_b64 = reply
            .data
            .get("audio")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let sample_rate = reply
            .data
            .get("sample_rate")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(crate::synth::vocab::KOKORO_SAMPLE_RATE))
            as u32;
        Ok(SynthResult {
            audio_b64,
            sample_rate,
        })
    }

    async fn resolve_conversation(&self, active_id: &str) -> Option<String> {
        if !active_id.is_empty() {
            return Some(active_id.to_owned());
        }
        // Fall back to the most recently updated conversation. The Rust
        // pipe doesn't own `conversations.list` yet (Phase 9 punchlist)
        // so this call goes to whichever harness impl is in front of the
        // pipe. A `no_action` or any error → None, mirroring Python's
        // best-effort fallback.
        let reply = send_action(&self.service, "conversations.list", json!({})).await;
        if !reply.ok {
            return None;
        }
        let arr = reply.data.get("conversations").and_then(Value::as_array)?;
        for entry in arr {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    return Some(id.to_owned());
                }
            }
        }
        None
    }
}

/// Wrap mono 16-bit PCM samples into an in-memory WAV byte buffer so the
/// in-process `voice.transcribe` handler can decode them (it reads a
/// self-describing WAV container, not raw PCM).
fn pcm_i16_to_wav(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|e| format!("WAV writer init: {e}"))?;
        for &s in samples {
            writer
                .write_sample(s)
                .map_err(|e| format!("WAV write sample: {e}"))?;
        }
        writer
            .finalize()
            .map_err(|e| format!("WAV finalize: {e}"))?;
    }
    Ok(cursor.into_inner())
}

fn format_reply_error(reply: &wylde_shared::ipc::Reply, action: &str) -> String {
    match reply.error.as_ref() {
        Some(e) => format!("{action} [{}] {}", e.code, e.message),
        None => format!("{action}: reply not ok with no error body"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_session_capture_reports_target_sample_rate() {
        let cap = MicSessionCapture::new();
        assert_eq!(cap.sample_rate(), TARGET_SAMPLE_RATE);
    }

    #[test]
    fn pcm_i16_to_wav_round_trips_via_decoder() {
        // The WAV we hand to `voice.transcribe` must decode cleanly back
        // to the same number of samples (the engine's `decode_wav` is the
        // real consumer).
        let samples = vec![0_i16, 1, -1, i16::MAX / 2, -(i16::MAX / 2)];
        let wav = pcm_i16_to_wav(&samples, TARGET_SAMPLE_RATE).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let pcm = crate::transcribe::audio::decode_wav(&wav).unwrap();
        assert_eq!(pcm.len(), samples.len());
    }

    #[test]
    fn format_reply_error_includes_code_and_message() {
        let reply = wylde_shared::ipc::Reply::err(wylde_shared::ipc::IpcError::new("foo", "bar"));
        assert_eq!(
            format_reply_error(&reply, "voice.transcribe"),
            "voice.transcribe [foo] bar"
        );
    }
}
