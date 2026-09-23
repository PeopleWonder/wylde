//! `voice.mic.*` — cpal microphone capture (Slice 11.D).
//!
//! Two unary verbs (`start` / `stop`) plus one streaming primitive
//! (`chunks`). The unary pair flips a process-wide singleton in
//! [`crate::state`]; the streaming verb subscribes to the active
//! singleton's broadcast and emits base64-encoded i16 PCM frames as
//! they arrive.
//!
//! Wire payloads:
//!
//! ```jsonc
//! // voice.mic.start
//! { "chunk_samples": 800 }   // optional, defaults to DEFAULT_MIC_CHUNK_SAMPLES
//!
//! // voice.mic.stop
//! null                        // takes no payload
//!
//! // voice.mic.chunks
//! null                        // no payload; streams base64 PCM chunks
//! ```

use std::sync::Arc;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::actions::error::invalid_request;
use crate::mic::{
    list_input_device_names, MicCapture, MicError, DEFAULT_MIC_CHUNK_SAMPLES, TARGET_SAMPLE_RATE,
};
use crate::state;
use crate::synth::wav::{encode_base64, encode_wav};

/// `voice.mic.start` — open the default input device and start the
/// chunk broadcast. Idempotent: a second call while a capture is
/// already running returns the existing capture's metadata with
/// `already_running: true`.
pub async fn handle_mic_start(payload: Value) -> Reply {
    let chunk_samples = match read_chunk_samples(&payload) {
        Ok(n) => n,
        Err(e) => return Reply::err(e),
    };

    if let Some(active) = state::mic_capture() {
        return Reply::ok(json!({
            "already_running": true,
            "chunk_samples": active.chunk_samples(),
            "sample_rate": TARGET_SAMPLE_RATE,
            "channels": 1,
            "input_sample_rate": active.input_sample_rate(),
            "input_channels": active.input_channels(),
        }));
    }

    let capture = match MicCapture::start(chunk_samples) {
        Ok(c) => c,
        Err(e) => return Reply::err(mic_error_to_ipc(e)),
    };
    let started = json!({
        "already_running": false,
        "chunk_samples": capture.chunk_samples(),
        "sample_rate": TARGET_SAMPLE_RATE,
        "channels": 1,
        "input_sample_rate": capture.input_sample_rate(),
        "input_channels": capture.input_channels(),
    });
    state::set_mic_capture(Arc::new(capture));
    Reply::ok(started)
}

/// `voice.mic.stop` — drop the active capture. Returns
/// `running: false` if there was nothing to stop.
pub async fn handle_mic_stop(_payload: Value) -> Reply {
    let was_running = state::take_mic_capture().is_some();
    Reply::ok(json!({
        "stopped": was_running,
        "was_running": was_running,
    }))
}

/// `voice.mic.chunks` — subscribe to the live PCM chunk broadcast.
/// Emits one `chunk` payload per mic frame and a `chunks_complete`
/// summary when the underlying capture stops or the client closes
/// the stream.
pub async fn handle_mic_chunks(_payload: Value, sender: StreamSender) {
    let Some(capture) = state::mic_capture() else {
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(invalid_request(
                "no active mic capture — call voice.mic.start first",
            )))
            .await;
        return;
    };

    let mut rx = capture.subscribe();
    let chunk_samples = capture.chunk_samples();
    let mut emitted: u64 = 0;
    let mut dropped: u64 = 0;

    loop {
        if sender.is_closed() {
            break;
        }
        match rx.recv().await {
            Ok(chunk) => {
                // Raw little-endian i16 PCM bytes — same wire format
                // Python's `SounddeviceCapture.capture()` emits.
                // Skipping the WAV wrapper here avoids the peak-normalise
                // step (`encode_wav` is for one-shot rendered audio); per
                // frame normalisation would amplify silence to full scale.
                let bytes = pcm_to_le_bytes(&chunk);
                let payload = json!({
                    "type": "chunk",
                    "seq": emitted,
                    "samples": chunk_samples,
                    "sample_rate": TARGET_SAMPLE_RATE,
                    "format": "pcm_s16le",
                    "audio_b64": encode_base64(&bytes),
                });
                if sender.send(Ok(payload)).await.is_err() {
                    break;
                }
                emitted += 1;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                dropped += n;
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    let _ = sender // wylde-check: discard-result-ok
        .send(Ok(json!({
            "type": "chunks_complete",
            "emitted": emitted,
            "dropped": dropped,
        })))
        .await;
}

/// `voice.list_input_devices` — enumerate the host's input devices for
/// the Settings → Voice mic-device picker (Slice 6). Reply:
/// `{default: <name|null>, devices: [<name>, ...]}`. Read-only — does
/// not open a stream or disturb an active capture.
pub async fn handle_list_input_devices(_payload: Value) -> Reply {
    match list_input_device_names() {
        Ok((default, devices)) => Reply::ok(json!({
            "default": default,
            "devices": devices,
        })),
        Err(e) => Reply::err(mic_error_to_ipc(e)),
    }
}

/// `voice.test_mic` — open a one-off capture (NOT the singleton),
/// collect a short window of audio, report its level, and attempt a
/// best-effort transcription (Slice 6's "Test mic" button).
///
/// Payload: `{capture_ms?}` (default 1500, clamped 300..=5000). Reply:
/// `{captured_ms, sample_rate, frames, rms, peak, transcript, note?}`.
///
/// Design notes:
/// * Uses a fresh [`MicCapture`] rather than the global singleton so it
///   never fights an in-flight `voice.mic.chunks` subscriber or wake-word
///   listener; the capture is dropped (stopping its worker) before reply.
/// * Transcription is best-effort: it reuses
///   [`crate::actions::transcribe::handle_transcribe`] but a missing
///   Whisper model (or any STT error) degrades to an empty `transcript`
///   plus a `note`, so the button still confirms the mic is live via the
///   level meter even when no model is installed.
pub async fn handle_test_mic(payload: Value) -> Reply {
    let capture_ms = payload
        .get("capture_ms")
        .and_then(Value::as_u64)
        .unwrap_or(1500)
        .clamp(300, 5000);

    let capture = match MicCapture::start(DEFAULT_MIC_CHUNK_SAMPLES) {
        Ok(c) => c,
        Err(e) => return Reply::err(mic_error_to_ipc(e)),
    };
    let mut rx = capture.subscribe();
    let mut collected: Vec<i16> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(capture_ms);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(chunk)) => collected.extend(chunk.iter().copied()),
            // Fell behind the broadcast — keep draining.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            // Capture closed underneath us, or the window elapsed.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => break,
        }
    }
    // Drop stops the worker thread + releases the OS device handle.
    drop(capture);

    let frames = collected.len();
    let (rms, peak) = level_stats(&collected);

    let mut transcript = String::new();
    let mut note: Option<String> = None;
    if frames == 0 {
        note = Some("no audio captured — check that your microphone is connected".to_owned());
    } else {
        let floats: Vec<f32> = collected
            .iter()
            .map(|&s| s as f32 / i16::MAX as f32)
            .collect();
        match encode_wav(&floats, TARGET_SAMPLE_RATE) {
            Ok(wav) => {
                let reply = crate::actions::transcribe::handle_transcribe(json!({
                    "audio_b64": encode_base64(&wav),
                }))
                .await;
                if reply.ok {
                    transcript = reply
                        .data
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                } else {
                    note = Some(
                        reply
                            .error
                            .map(|e| format!("{}: {}", e.code, e.message))
                            .unwrap_or_else(|| "transcription unavailable".to_owned()),
                    );
                }
            }
            Err(e) => note = Some(format!("wav encode failed: {e}")),
        }
    }

    Reply::ok(json!({
        "captured_ms": capture_ms,
        "sample_rate": TARGET_SAMPLE_RATE,
        "frames": frames,
        "rms": rms,
        "peak": peak,
        "transcript": transcript,
        "note": note,
    }))
}

/// RMS + peak of a 16-bit PCM buffer, normalised to the `[0.0, 1.0]`
/// full-scale range. Pure helper so the level math is unit-testable
/// without a live device.
fn level_stats(samples: &[i16]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum_sq = 0.0_f64;
    let mut peak = 0.0_f32;
    for &s in samples {
        let f = s as f32 / i16::MAX as f32;
        sum_sq += (f as f64) * (f as f64);
        peak = peak.max(f.abs());
    }
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    (rms, peak)
}

fn read_chunk_samples(payload: &Value) -> Result<usize, IpcError> {
    match payload.get("chunk_samples") {
        None | Some(Value::Null) => Ok(DEFAULT_MIC_CHUNK_SAMPLES),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| invalid_request("chunk_samples must be a positive integer"))?;
            if n == 0 {
                return Err(invalid_request("chunk_samples must be > 0"));
            }
            if n > 32_000 {
                return Err(invalid_request(
                    "chunk_samples must be ≤ 32000 (2 s at 16 kHz)",
                ));
            }
            Ok(n as usize)
        }
    }
}

fn pcm_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub(crate) fn mic_error_to_ipc(e: MicError) -> IpcError {
    match e {
        MicError::NoDevice => IpcError::new("mic_unavailable", "no default input device"),
        MicError::NoSupportedConfig(m) => IpcError::new(
            "mic_unavailable",
            format!("default input has no supported config: {m}"),
        ),
        MicError::Build(m) => IpcError::new("mic_unavailable", format!("cpal stream build: {m}")),
        MicError::Play(m) => IpcError::new("mic_unavailable", format!("cpal stream play: {m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn mic_stop_when_not_running_returns_false() {
        crate::state::reset_for_tests();
        let r = handle_mic_stop(json!({})).await;
        assert!(r.ok);
        assert_eq!(r.data["stopped"], false);
        assert_eq!(r.data["was_running"], false);
    }

    #[tokio::test]
    async fn mic_chunks_without_active_capture_errors() {
        crate::state::reset_for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_mic_chunks(json!({}), tx).await;
        let chunk = rx.recv().await.expect("error chunk emitted");
        let err = chunk.expect_err("missing capture → stream-level error");
        assert_eq!(err.code, "invalid_request");
        assert!(err.message.contains("voice.mic.start"));
    }

    #[test]
    fn read_chunk_samples_uses_default_when_missing() {
        assert_eq!(
            read_chunk_samples(&json!({})).unwrap(),
            DEFAULT_MIC_CHUNK_SAMPLES
        );
        assert_eq!(
            read_chunk_samples(&json!({"chunk_samples": null})).unwrap(),
            DEFAULT_MIC_CHUNK_SAMPLES
        );
    }

    #[test]
    fn read_chunk_samples_rejects_zero() {
        let err = read_chunk_samples(&json!({"chunk_samples": 0})).unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn read_chunk_samples_rejects_overflow() {
        let err = read_chunk_samples(&json!({"chunk_samples": 99_999})).unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn read_chunk_samples_accepts_legit_values() {
        assert_eq!(
            read_chunk_samples(&json!({"chunk_samples": 1_280})).unwrap(),
            1_280
        );
        assert_eq!(
            read_chunk_samples(&json!({"chunk_samples": 800})).unwrap(),
            800
        );
    }

    #[test]
    fn mic_error_maps_to_mic_unavailable() {
        let e = mic_error_to_ipc(MicError::NoDevice);
        assert_eq!(e.code, "mic_unavailable");
        let e = mic_error_to_ipc(MicError::Build("x".into()));
        assert_eq!(e.code, "mic_unavailable");
    }

    #[test]
    fn level_stats_zero_for_silence() {
        let (rms, peak) = level_stats(&[0; 256]);
        assert_eq!(rms, 0.0);
        assert_eq!(peak, 0.0);
        // Empty buffer is also (0, 0), not a divide-by-zero.
        assert_eq!(level_stats(&[]), (0.0, 0.0));
    }

    #[test]
    fn level_stats_full_scale_tone() {
        // Alternating +/- full scale: peak = 1.0, rms = 1.0.
        let buf: Vec<i16> = (0..256)
            .map(|i| if i % 2 == 0 { i16::MAX } else { -i16::MAX })
            .collect();
        let (rms, peak) = level_stats(&buf);
        assert!((peak - 1.0).abs() < 1e-6, "peak {peak}");
        assert!((rms - 1.0).abs() < 1e-3, "rms {rms}");
    }

    // `#[ignore]` for the same reason as the capture/playback device tests
    // (mic.rs / playback.rs): it enumerates the host's input devices via
    // `cpal::default_host()`, and on a headless CI runner WASAPI enumeration
    // *access-violates* (STATUS_ACCESS_VIOLATION) rather than returning a
    // catchable `mic_unavailable` — a native crash safe Rust can't intercept,
    // so the graceful-reply assertion below never gets to run. Runs locally
    // (`cargo test -- --ignored`) where a real audio device exists.
    #[tokio::test]
    #[ignore = "requires a working default input device; cpal enumeration access-violates on headless CI"]
    async fn list_input_devices_dispatches_cleanly() {
        // Either a device list (Ok) or a mic_unavailable error on a host with a
        // real (but unusable) device — both are well-formed replies.
        let r = handle_list_input_devices(json!({})).await;
        if r.ok {
            assert!(r.data["devices"].is_array());
            // `default` is a string or null.
            assert!(r.data.get("default").is_some());
        } else {
            assert_eq!(r.error.unwrap().code, "mic_unavailable");
        }
    }

    #[test]
    fn pcm_to_le_bytes_is_byte_perfect() {
        let samples: Vec<i16> = vec![0, 1, -1, 256, -256, i16::MAX, i16::MIN];
        let bytes = pcm_to_le_bytes(&samples);
        assert_eq!(bytes.len(), samples.len() * 2);
        for (i, &s) in samples.iter().enumerate() {
            let lo = bytes[i * 2];
            let hi = bytes[i * 2 + 1];
            assert_eq!(s, i16::from_le_bytes([lo, hi]));
        }
    }
}
