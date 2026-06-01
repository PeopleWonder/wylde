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
use crate::mic::{DEFAULT_MIC_CHUNK_SAMPLES, MicCapture, MicError, TARGET_SAMPLE_RATE};
use crate::state;
use crate::synth::wav::encode_base64;

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
                return Err(invalid_request("chunk_samples must be ≤ 32000 (2 s at 16 kHz)"));
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
        MicError::NoDevice => {
            IpcError::new("mic_unavailable", "no default input device")
        }
        MicError::NoSupportedConfig(m) => {
            IpcError::new("mic_unavailable", format!("default input has no supported config: {m}"))
        }
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
        assert_eq!(read_chunk_samples(&json!({})).unwrap(), DEFAULT_MIC_CHUNK_SAMPLES);
        assert_eq!(read_chunk_samples(&json!({"chunk_samples": null})).unwrap(), DEFAULT_MIC_CHUNK_SAMPLES);
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
        assert_eq!(read_chunk_samples(&json!({"chunk_samples": 1_280})).unwrap(), 1_280);
        assert_eq!(read_chunk_samples(&json!({"chunk_samples": 800})).unwrap(), 800);
    }

    #[test]
    fn mic_error_maps_to_mic_unavailable() {
        let e = mic_error_to_ipc(MicError::NoDevice);
        assert_eq!(e.code, "mic_unavailable");
        let e = mic_error_to_ipc(MicError::Build("x".into()));
        assert_eq!(e.code, "mic_unavailable");
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
