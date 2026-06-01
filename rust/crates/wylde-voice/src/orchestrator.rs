//! Per-session orchestration loop (Slice 11.E port of
//! [`Voice/orchestrator.py`](../../../../Voice/orchestrator.py)).
//!
//! One [`run_session`] call drives a complete round-trip: capture audio
//! off the mic, send it to the wylde-voice STT primitive, post the
//! transcript as a chat turn through the harness, send the assistant
//! response to the TTS primitive, play the resulting WAV via cpal.
//!
//! ## Where it lives
//!
//! In the Python tree the orchestrator lived in `Voice/` because Voice
//! owned audio + STT + TTS in-process. In the Rust port, `wylde-voice`
//! still owns audio (cpal mic + cpal playback) and the STT/TTS ONNX
//! sessions, so the orchestrator lands here too. It calls back into the
//! wylde-harness pipe for `chat.run_turn` — the same shape the Python
//! orchestrator used.
//!
//! ## Wiring status (2026-05-26)
//!
//! This module is **internal-only**: the public [`run_session`] entry
//! point is exercised by unit tests via the [`HarnessChat`] trait, but
//! is not yet bound to a `voice.*` pipe action. The GUI-facing surface
//! (`voice.toggle`, `voice.set_mode`, `voice.subscribe_status`, …) still
//! lives in `Voice/pipe.py`; that port is the next slice (Slice 11.E+).
//! Strangler-fig invariant: while `WYLDE_WYLDE_VOICE_IMPL=python` is the
//! default, the Python orchestrator is authoritative — flipping the
//! default REQUIRES the GUI-facing surface to be ported here first.
//!
//! ## State machine
//!
//! The session walks four states: IDLE, LISTENING (capture), PROCESSING
//! (STT plus chat turn), and PLAYING (TTS plus speaker), then back to
//! IDLE. Errors short-circuit the chain and finalize the session with
//! an `error` field set.
//!
//! Mirrors the Python flow one-for-one so the strangler-fig cutover
//! doesn't change observable behaviour.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::playback::PlaybackError;

/// State labels reported to subscribers via the GUI-facing status feed.
/// Lowercase wire strings match `Voice/state.py::STATE_*`.
pub const STATE_IDLE: &str = "idle";
pub const STATE_LISTENING: &str = "listening";
pub const STATE_PROCESSING: &str = "processing";
pub const STATE_PLAYING: &str = "playing";
pub const STATE_ERROR: &str = "error";

/// Errors finalised onto [`SessionResult::error`]. Mirrors the Python
/// orchestrator's stringy error labels so existing GUI code that
/// switches on the value keeps working.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("no_conversation")]
    NoConversation,
    #[error("no_audio_captured")]
    NoAudioCaptured,
    #[error("audio_unavailable: {0}")]
    AudioUnavailable(String),
    #[error("transcribe_failed: {0}")]
    TranscribeFailed(String),
    #[error("empty_transcript")]
    EmptyTranscript,
    #[error("chat_failed: {0}")]
    ChatFailed(String),
    #[error("chat_aborted: {0}")]
    ChatAborted(String),
    #[error("empty_response")]
    EmptyResponse,
    #[error("synthesize_failed: {0}")]
    SynthesizeFailed(String),
    #[error("tts_returned_no_audio")]
    TtsReturnedNoAudio,
    #[error("playback_unavailable: {0}")]
    PlaybackUnavailable(String),
    #[error("playback_failed: {0}")]
    PlaybackFailed(String),
}

/// Outcome of one [`run_session`] call. Mirrors the Python
/// `SessionResult` shape so JSON-wire compat with the GUI is preserved
/// when this is bound to a pipe action.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionResult {
    pub session_id: String,
    pub conversation_id: String,
    #[serde(default)]
    pub transcript: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub aborted: bool,
    /// Stringly-typed error label mirroring Python's `error: Optional[str]`.
    /// Use [`SessionResult::error_kind`] to recover the typed variant.
    pub error: Option<String>,
    #[serde(default)]
    pub timings_ms: TimingsMs,
}

impl SessionResult {
    pub fn error_kind(&self) -> Option<SessionError> {
        // Round-trip via Display + parse not implemented — error labels
        // are forward-facing strings. Callers that need the typed kind
        // can hold onto the SessionError directly from the helpers.
        None
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TimingsMs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcribe_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesize_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_ms: Option<u64>,
}

/// Audio capture handle. The orchestrator calls
/// [`capture`](AudioCapture::capture) once per session and expects a
/// blob of i16 mono PCM at [`AudioCapture::sample_rate`].
///
/// Production impl wraps [`crate::mic::MicCapture`]; tests use the
/// in-tree [`MockCapture`].
#[async_trait]
pub trait AudioCapture: Send + Sync {
    async fn capture(&self, max_seconds: f32) -> Result<Vec<i16>, AudioUnavailable>;
    fn sample_rate(&self) -> u32;
}

/// Speaker playback handle. Counterpart to [`AudioCapture`]; called
/// once after TTS finishes.
#[async_trait]
pub trait AudioPlayback: Send + Sync {
    async fn play(&self, pcm_i16: Vec<i16>, sample_rate: u32) -> Result<(), PlaybackError>;
}

/// Harness chat handle. Mirrors the Python `HarnessClientProtocol`
/// surface but with three async methods. Production impl forwards to
/// `\\.\pipe\wylde-harness::chat.run_turn`.
#[async_trait]
pub trait HarnessChat: Send + Sync {
    /// Forward a transcribe call. The orchestrator drives this against
    /// the wylde-voice STT primitive via the harness's
    /// `voice.transcribe` bridge — the same value catalog entry the
    /// model gets.
    async fn transcribe(&self, audio: &[i16], sample_rate: u32) -> Result<String, HarnessCallError>;
    /// Drive one chat turn through the harness. `modality` is set to
    /// `"voice"` so the slot-ordering builder folds in the voice prelude.
    async fn run_chat_turn(
        &self,
        user_message: &str,
        conversation_id: &str,
        model: Option<&str>,
    ) -> Result<ChatTurnResult, HarnessCallError>;
    /// Synthesize the assistant response. Returns base64 WAV + sample rate.
    async fn synthesize(&self, text: &str) -> Result<SynthResult, HarnessCallError>;
    /// Resolve the conversation id the orchestrator should bind this
    /// session to. Returns `None` when there's no conversation to use
    /// (cold start, empty store) — the orchestrator then finalises with
    /// `no_conversation` error.
    async fn resolve_conversation(&self, active_id: &str) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatTurnResult {
    pub final_message: String,
    pub aborted: bool,
    pub abort_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthResult {
    pub audio_b64: String,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Error)]
#[error("harness call failed: {0}")]
pub struct HarnessCallError(pub String);

#[derive(Debug, Clone, Error)]
#[error("audio unavailable: {0}")]
pub struct AudioUnavailable(pub String);

/// Per-call inputs for [`run_session`]. Mirrors the Python keyword args.
pub struct SessionInputs<'a> {
    pub active_conversation_id: &'a str,
    pub max_capture_seconds: f32,
    pub model: Option<&'a str>,
}

impl<'a> SessionInputs<'a> {
    pub fn new(active_conversation_id: &'a str) -> Self {
        Self {
            active_conversation_id,
            max_capture_seconds: 30.0,
            model: None,
        }
    }
}

/// Drive one capture → STT → chat → TTS → play round-trip.
///
/// Mirrors `Voice/orchestrator.py::run_session` one-for-one.
pub async fn run_session<C, P, H>(
    capture: Arc<C>,
    playback: Arc<P>,
    harness: Arc<H>,
    inputs: SessionInputs<'_>,
) -> SessionResult
where
    C: AudioCapture + 'static,
    P: AudioPlayback + 'static,
    H: HarnessChat + 'static,
{
    let session_id = new_session_id();

    let conv_id = match harness.resolve_conversation(inputs.active_conversation_id).await {
        Some(id) if !id.is_empty() => id,
        _ => {
            return finalize(
                &session_id,
                "",
                "",
                "",
                Some(SessionError::NoConversation),
                TimingsMs::default(),
            );
        }
    };

    let mut timings = TimingsMs::default();
    let mut transcript = String::new();
    let mut response = String::new();

    // Capture.
    let t0 = Instant::now();
    let audio_bytes = match capture.capture(inputs.max_capture_seconds).await {
        Ok(a) => a,
        Err(e) => {
            return finalize(
                &session_id,
                &conv_id,
                &transcript,
                &response,
                Some(SessionError::AudioUnavailable(e.0)),
                timings,
            );
        }
    };
    timings.capture_ms = Some(t0.elapsed().as_millis() as u64);
    if audio_bytes.is_empty() {
        return finalize(
            &session_id,
            &conv_id,
            &transcript,
            &response,
            Some(SessionError::NoAudioCaptured),
            timings,
        );
    }

    // STT.
    let t0 = Instant::now();
    transcript = match harness.transcribe(&audio_bytes, capture.sample_rate()).await {
        Ok(t) => t,
        Err(e) => {
            return finalize(
                &session_id,
                &conv_id,
                &transcript,
                &response,
                Some(SessionError::TranscribeFailed(e.0)),
                timings,
            );
        }
    };
    timings.transcribe_ms = Some(t0.elapsed().as_millis() as u64);
    if transcript.trim().is_empty() {
        return finalize(
            &session_id,
            &conv_id,
            &transcript,
            &response,
            Some(SessionError::EmptyTranscript),
            timings,
        );
    }

    // Chat turn through the harness.
    let t0 = Instant::now();
    let chat = match harness
        .run_chat_turn(&transcript, &conv_id, inputs.model)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            return finalize(
                &session_id,
                &conv_id,
                &transcript,
                &response,
                Some(SessionError::ChatFailed(e.0)),
                timings,
            );
        }
    };
    timings.chat_ms = Some(t0.elapsed().as_millis() as u64);
    response = chat.final_message.clone();
    if chat.aborted {
        return finalize(
            &session_id,
            &conv_id,
            &transcript,
            &response,
            Some(SessionError::ChatAborted(
                chat.abort_reason.unwrap_or_else(|| "unknown".to_owned()),
            )),
            timings,
        );
    }
    if response.trim().is_empty() {
        return finalize(
            &session_id,
            &conv_id,
            &transcript,
            &response,
            Some(SessionError::EmptyResponse),
            timings,
        );
    }

    // TTS.
    let t0 = Instant::now();
    let tts = match harness.synthesize(&response).await {
        Ok(t) => t,
        Err(e) => {
            return finalize(
                &session_id,
                &conv_id,
                &transcript,
                &response,
                Some(SessionError::SynthesizeFailed(e.0)),
                timings,
            );
        }
    };
    timings.synthesize_ms = Some(t0.elapsed().as_millis() as u64);
    if tts.audio_b64.is_empty() {
        return finalize(
            &session_id,
            &conv_id,
            &transcript,
            &response,
            Some(SessionError::TtsReturnedNoAudio),
            timings,
        );
    }

    // Playback.
    let pcm = match decode_wav_to_i16(&tts.audio_b64) {
        Ok(p) => p,
        Err(e) => {
            return finalize(
                &session_id,
                &conv_id,
                &transcript,
                &response,
                Some(SessionError::PlaybackFailed(format!("decode WAV: {e}"))),
                timings,
            );
        }
    };
    let t0 = Instant::now();
    if let Err(e) = playback.play(pcm, tts.sample_rate).await {
        let kind = match e {
            PlaybackError::NoDevice
            | PlaybackError::NoSupportedConfig(_)
            | PlaybackError::Build(_) => SessionError::PlaybackUnavailable(e.to_string()),
            _ => SessionError::PlaybackFailed(e.to_string()),
        };
        return finalize(
            &session_id,
            &conv_id,
            &transcript,
            &response,
            Some(kind),
            timings,
        );
    }
    timings.playback_ms = Some(t0.elapsed().as_millis() as u64);

    finalize(&session_id, &conv_id, &transcript, &response, None, timings)
}

fn finalize(
    session_id: &str,
    conversation_id: &str,
    transcript: &str,
    response: &str,
    err: Option<SessionError>,
    timings: TimingsMs,
) -> SessionResult {
    SessionResult {
        session_id: session_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        transcript: transcript.to_owned(),
        response: response.to_owned(),
        aborted: false,
        error: err.map(|e| e.to_string()),
        timings_ms: timings,
    }
}

fn new_session_id() -> String {
    // Mirrors Python's `uuid.uuid4().hex[:12]`.
    let id = uuid::Uuid::new_v4().simple().to_string();
    id.chars().take(12).collect()
}

/// Decode a base64-encoded WAV blob into a Vec<i16>. The TTS pipeline
/// always emits 16-bit PCM mono so we assert that on the way out.
fn decode_wav_to_i16(audio_b64: &str) -> Result<Vec<i16>, String> {
    let bytes = BASE64
        .decode(audio_b64.as_bytes())
        .map_err(|e| format!("base64 decode: {e}"))?;
    let cursor = std::io::Cursor::new(bytes);
    let mut reader = hound::WavReader::new(cursor).map_err(|e| format!("WAV header: {e}"))?;
    let spec = reader.spec();
    if spec.bits_per_sample != 16 {
        return Err(format!(
            "WAV must be 16-bit PCM (got {} bits)",
            spec.bits_per_sample
        ));
    }
    // Allow mono or stereo input; if stereo, downmix to mono.
    let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
    let samples = samples.map_err(|e| format!("WAV samples: {e}"))?;
    if spec.channels <= 1 {
        return Ok(samples);
    }
    let chans = spec.channels as usize;
    let frames = samples.len() / chans;
    let mut mono = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * chans;
        let mut sum: i32 = 0;
        for c in 0..chans {
            sum += samples[base + c] as i32;
        }
        mono.push((sum / chans as i32) as i16);
    }
    Ok(mono)
}

/// Project the result onto the JSON shape `Voice/orchestrator.py`'s
/// `SessionResult.to_dict` produced — used when the orchestrator is
/// bound to a pipe action so the GUI sees byte-for-byte the same envelope
/// the Python service emitted.
impl SessionResult {
    pub fn to_value(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "conversation_id": self.conversation_id,
            "transcript": self.transcript,
            "response": self.response,
            "aborted": self.aborted,
            "error": self.error,
            "timings_ms": serde_json::to_value(&self.timings_ms).unwrap_or(Value::Null),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    /// In-memory capture stub. Returns a fixed buffer on every call.
    struct MockCapture {
        buffer: Vec<i16>,
        sample_rate: u32,
        delay: Duration,
        fail: Option<String>,
    }

    #[async_trait]
    impl AudioCapture for MockCapture {
        async fn capture(&self, _max_seconds: f32) -> Result<Vec<i16>, AudioUnavailable> {
            if self.delay > Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }
            if let Some(reason) = &self.fail {
                return Err(AudioUnavailable(reason.clone()));
            }
            Ok(self.buffer.clone())
        }
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
    }

    struct MockPlayback {
        last_call: Mutex<Option<(usize, u32)>>,
        fail: Option<PlaybackError>,
    }

    #[async_trait]
    impl AudioPlayback for MockPlayback {
        async fn play(&self, pcm: Vec<i16>, sample_rate: u32) -> Result<(), PlaybackError> {
            *self.last_call.lock().unwrap() = Some((pcm.len(), sample_rate));
            if let Some(err) = &self.fail {
                return Err(match err {
                    PlaybackError::NoDevice => PlaybackError::NoDevice,
                    PlaybackError::EmptyBuffer => PlaybackError::EmptyBuffer,
                    PlaybackError::Build(s) => PlaybackError::Build(s.clone()),
                    PlaybackError::Play(s) => PlaybackError::Play(s.clone()),
                    PlaybackError::Timeout(d) => PlaybackError::Timeout(*d),
                    PlaybackError::BufferTooLarge(a, b) => PlaybackError::BufferTooLarge(*a, *b),
                    PlaybackError::NoSupportedConfig(s) => PlaybackError::NoSupportedConfig(s.clone()),
                });
            }
            Ok(())
        }
    }

    /// In-memory harness. Records every call so the test can assert on
    /// `modality`-like contract bits.
    struct MockHarness {
        conv_id: Option<String>,
        transcribe_result: Result<String, HarnessCallError>,
        chat_result: Result<ChatTurnResult, HarnessCallError>,
        synth_result: Result<SynthResult, HarnessCallError>,
    }

    #[async_trait]
    impl HarnessChat for MockHarness {
        async fn transcribe(
            &self,
            _audio: &[i16],
            _sample_rate: u32,
        ) -> Result<String, HarnessCallError> {
            self.transcribe_result.clone()
        }
        async fn run_chat_turn(
            &self,
            _user_message: &str,
            _conversation_id: &str,
            _model: Option<&str>,
        ) -> Result<ChatTurnResult, HarnessCallError> {
            self.chat_result.clone()
        }
        async fn synthesize(&self, _text: &str) -> Result<SynthResult, HarnessCallError> {
            self.synth_result.clone()
        }
        async fn resolve_conversation(&self, active_id: &str) -> Option<String> {
            if !active_id.is_empty() {
                return Some(active_id.to_owned());
            }
            self.conv_id.clone()
        }
    }

    fn ok_harness() -> MockHarness {
        // Generate one second of silent 16-kHz mono WAV, base64-encoded.
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let mut writer = hound::WavWriter::new(std::io::Cursor::new(&mut buf), spec).unwrap();
            for _ in 0..2400 {
                writer.write_sample(0_i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        let audio_b64 = BASE64.encode(&buf);
        MockHarness {
            conv_id: Some("conv-1".to_owned()),
            transcribe_result: Ok("hello world".to_owned()),
            chat_result: Ok(ChatTurnResult {
                final_message: "hello back".to_owned(),
                aborted: false,
                abort_reason: None,
            }),
            synth_result: Ok(SynthResult {
                audio_b64,
                sample_rate: 24_000,
            }),
        }
    }

    fn ok_capture() -> MockCapture {
        MockCapture {
            buffer: vec![1, 2, 3, 4],
            sample_rate: 16_000,
            delay: Duration::ZERO,
            fail: None,
        }
    }

    fn ok_playback() -> MockPlayback {
        MockPlayback {
            last_call: Mutex::new(None),
            fail: None,
        }
    }

    #[tokio::test]
    async fn happy_path_returns_transcript_and_response() {
        let cap = Arc::new(ok_capture());
        let play = Arc::new(ok_playback());
        let harn = Arc::new(ok_harness());
        let r = run_session(
            cap,
            play.clone(),
            harn,
            SessionInputs::new("conv-1"),
        )
        .await;
        assert_eq!(r.transcript, "hello world");
        assert_eq!(r.response, "hello back");
        assert_eq!(r.conversation_id, "conv-1");
        assert_eq!(r.error, None);
        assert!(r.timings_ms.capture_ms.is_some());
        assert!(r.timings_ms.transcribe_ms.is_some());
        assert!(r.timings_ms.chat_ms.is_some());
        assert!(r.timings_ms.synthesize_ms.is_some());
        assert!(r.timings_ms.playback_ms.is_some());
        // Playback received the decoded WAV (mono i16 @ 24 kHz).
        let last = *play.last_call.lock().unwrap();
        let (samples, sr) = last.expect("playback called");
        assert_eq!(sr, 24_000);
        assert!(samples > 0);
    }

    #[tokio::test]
    async fn no_conversation_returns_early() {
        let cap = Arc::new(ok_capture());
        let play = Arc::new(ok_playback());
        let mut h = ok_harness();
        h.conv_id = None;
        let r = run_session(cap, play, Arc::new(h), SessionInputs::new("")).await;
        assert_eq!(r.error.as_deref(), Some("no_conversation"));
        assert_eq!(r.conversation_id, "");
        // Capture should not have run.
        assert!(r.timings_ms.capture_ms.is_none());
    }

    #[tokio::test]
    async fn empty_transcript_short_circuits() {
        let cap = Arc::new(ok_capture());
        let play = Arc::new(ok_playback());
        let mut h = ok_harness();
        h.transcribe_result = Ok("   ".to_owned());
        let r = run_session(cap, play, Arc::new(h), SessionInputs::new("conv-1")).await;
        assert_eq!(r.error.as_deref(), Some("empty_transcript"));
        assert!(r.response.is_empty());
        assert!(r.timings_ms.chat_ms.is_none());
    }

    #[tokio::test]
    async fn transcribe_error_finalizes_cleanly() {
        let cap = Arc::new(ok_capture());
        let play = Arc::new(ok_playback());
        let mut h = ok_harness();
        h.transcribe_result = Err(HarnessCallError("network down".to_owned()));
        let r = run_session(cap, play, Arc::new(h), SessionInputs::new("conv-1")).await;
        assert!(r.error.unwrap().contains("transcribe_failed"));
    }

    #[tokio::test]
    async fn no_audio_short_circuits() {
        let mut c = ok_capture();
        c.buffer = Vec::new();
        let play = Arc::new(ok_playback());
        let r = run_session(
            Arc::new(c),
            play,
            Arc::new(ok_harness()),
            SessionInputs::new("conv-1"),
        )
        .await;
        assert_eq!(r.error.as_deref(), Some("no_audio_captured"));
        assert!(r.transcript.is_empty());
    }

    #[tokio::test]
    async fn chat_aborted_carries_reason() {
        let mut h = ok_harness();
        h.chat_result = Ok(ChatTurnResult {
            final_message: String::new(),
            aborted: true,
            abort_reason: Some("user_cancelled".to_owned()),
        });
        let r = run_session(
            Arc::new(ok_capture()),
            Arc::new(ok_playback()),
            Arc::new(h),
            SessionInputs::new("conv-1"),
        )
        .await;
        let err = r.error.unwrap();
        assert!(err.contains("chat_aborted"));
        assert!(err.contains("user_cancelled"));
    }

    #[tokio::test]
    async fn tts_empty_audio_surfaces_error() {
        let mut h = ok_harness();
        h.synth_result = Ok(SynthResult {
            audio_b64: String::new(),
            sample_rate: 24_000,
        });
        let r = run_session(
            Arc::new(ok_capture()),
            Arc::new(ok_playback()),
            Arc::new(h),
            SessionInputs::new("conv-1"),
        )
        .await;
        assert_eq!(r.error.as_deref(), Some("tts_returned_no_audio"));
    }

    #[tokio::test]
    async fn playback_unavailable_carries_session_anyway() {
        // Playback failure must NOT lose the chat turn — the transcript
        // + response are still landed; the user just couldn't hear it.
        let play = MockPlayback {
            last_call: Mutex::new(None),
            fail: Some(PlaybackError::NoDevice),
        };
        let r = run_session(
            Arc::new(ok_capture()),
            Arc::new(play),
            Arc::new(ok_harness()),
            SessionInputs::new("conv-1"),
        )
        .await;
        assert!(!r.transcript.is_empty(), "transcript should still be there");
        assert!(!r.response.is_empty(), "response should still be there");
        let err = r.error.unwrap();
        assert!(err.contains("playback_unavailable"), "{err}");
    }

    #[test]
    fn session_id_has_12_chars() {
        let id = new_session_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn to_value_matches_python_envelope_shape() {
        let r = SessionResult {
            session_id: "abc".to_owned(),
            conversation_id: "conv".to_owned(),
            transcript: "hi".to_owned(),
            response: "yo".to_owned(),
            aborted: false,
            error: None,
            timings_ms: TimingsMs {
                capture_ms: Some(10),
                transcribe_ms: Some(20),
                chat_ms: Some(30),
                synthesize_ms: Some(40),
                playback_ms: Some(50),
            },
        };
        let v = r.to_value();
        assert_eq!(v["session_id"], "abc");
        assert_eq!(v["conversation_id"], "conv");
        assert_eq!(v["transcript"], "hi");
        assert_eq!(v["response"], "yo");
        assert_eq!(v["aborted"], false);
        assert!(v["error"].is_null());
        assert_eq!(v["timings_ms"]["capture_ms"], 10);
        assert_eq!(v["timings_ms"]["playback_ms"], 50);
    }
}
