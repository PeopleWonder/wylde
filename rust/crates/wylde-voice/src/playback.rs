//! Speaker playback (Slice 11.E).
//!
//! Opens the default output device via `cpal` and plays a chunk of i16
//! mono PCM at a caller-supplied sample rate. Counterpart to [`crate::mic`]
//! on the output side — same dedicated-worker-thread ownership story so
//! the !Send + !Sync `cpal::Stream` is built and dropped on the same
//! thread the audio driver expects.
//!
//! ## Why a fire-and-forget worker thread (not a daemon singleton)
//!
//! The synth pipeline produces a complete WAV per utterance — there is
//! no continuous PCM source on the speaker side that mirrors the mic's
//! broadcast model. Each [`play_blocking`] call:
//!
//! 1. Decodes the i16 PCM into f32 in the calling task,
//! 2. Spawns a worker that opens the default output device, plays the
//!    buffer to completion, and drops the stream,
//! 3. Joins the worker so the caller's `await` resolves only after the
//!    last sample has been written to the device buffer.
//!
//! No singleton state — repeat calls just open a fresh stream. The
//! Python predecessor `Voice/audio_io.py::SounddevicePlayback` follows
//! the same one-shot pattern; we preserve it so the strangler-fig
//! cutover doesn't change observable behaviour.
//!
//! ## Format
//!
//! Input: i16 mono PCM at the caller's sample rate.
//! Device: requests an f32 stream at the caller's sample rate when the
//! device's `default_output_config` is f32 + matching rate. Otherwise we
//! fall back to the device's supported rate and naive-linear-resample
//! to it — same approach `mic::resample_to_16k` uses on the input side
//! but in the opposite direction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use thiserror::Error;

/// Reasonable safety cap on a single playback call (60 s of 24 kHz
/// mono i16 = 2.88 MB; we cap at twice that). The orchestrator's TTS
/// path produces ~3 s utterances; anything longer than 60 s is almost
/// certainly a bug.
const MAX_PLAYBACK_SECONDS: f32 = 60.0;

#[derive(Debug, Error)]
pub enum PlaybackError {
    #[error("no default output device available")]
    NoDevice,

    #[error("output device has no supported config: {0}")]
    NoSupportedConfig(String),

    #[error("cpal stream build failed: {0}")]
    Build(String),

    #[error("cpal stream play failed: {0}")]
    Play(String),

    #[error("playback timed out after {0:?}")]
    Timeout(Duration),

    #[error("empty audio buffer")]
    EmptyBuffer,

    #[error("audio buffer exceeds safety cap ({0:.1}s > {1:.1}s)")]
    BufferTooLarge(f32, f32),
}

/// Play `pcm_i16` mono PCM at `sample_rate` Hz to the default output
/// device, blocking until the device buffer drains.
///
/// Async-friendly: the cpal-owning work happens on a dedicated
/// `std::thread::spawn`; the caller's `await` resolves when the worker
/// joins. Implemented via `tokio::task::spawn_blocking` wrapping the
/// std-thread spawn so the tokio runtime accounts for the blocking
/// thread correctly.
pub async fn play_blocking(pcm_i16: Vec<i16>, sample_rate: u32) -> Result<(), PlaybackError> {
    if pcm_i16.is_empty() {
        return Err(PlaybackError::EmptyBuffer);
    }
    if sample_rate == 0 {
        return Err(PlaybackError::Build("sample_rate must be > 0".to_owned()));
    }
    let audio_seconds = pcm_i16.len() as f32 / sample_rate as f32;
    if audio_seconds > MAX_PLAYBACK_SECONDS {
        return Err(PlaybackError::BufferTooLarge(
            audio_seconds,
            MAX_PLAYBACK_SECONDS,
        ));
    }

    // Move the cpal work onto a blocking thread so the tokio runtime
    // can schedule other tasks. The thread inside the blocking task is
    // the std::thread that actually owns the stream (cpal::Stream is
    // !Send so we can't poll-and-yield in-place).
    let join_result = tokio::task::spawn_blocking(move || run_playback(pcm_i16, sample_rate))
        .await
        .map_err(|e| PlaybackError::Play(format!("blocking task panicked: {e}")))?;
    join_result
}

fn run_playback(pcm_i16: Vec<i16>, sample_rate: u32) -> Result<(), PlaybackError> {
    // Convert to f32 once in the calling thread so the audio-driver
    // callback stays branch-free.
    let pcm_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / i16::MAX as f32).collect();

    let done = Arc::new(AtomicBool::new(false));
    let done_for_worker = Arc::clone(&done);
    let total_samples = pcm_f32.len();

    let worker = thread::Builder::new()
        .name("wylde-voice-playback".to_owned())
        .spawn(move || open_and_play(pcm_f32, sample_rate, done_for_worker, total_samples))
        .map_err(|e| PlaybackError::Build(format!("spawn worker: {e}")))?;

    worker
        .join()
        .map_err(|_| PlaybackError::Play("playback worker panicked".to_owned()))?
}

fn open_and_play(
    pcm_f32: Vec<f32>,
    requested_rate: u32,
    done: Arc<AtomicBool>,
    total_samples: usize,
) -> Result<(), PlaybackError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(PlaybackError::NoDevice)?;
    let supported = device
        .default_output_config()
        .map_err(|e| PlaybackError::NoSupportedConfig(e.to_string()))?;

    let device_rate = supported.sample_rate().0;
    let device_channels = supported.channels();
    let sample_format = supported.sample_format();

    // Naive linear resample if device rate differs from the caller's
    // sample rate. Output side is the symmetric counterpart of
    // `mic::resample_to_16k` — acceptable for short utterances; a
    // future polyphase swap is tracked in the Slice 11.E+ punchlist.
    let resampled = if device_rate == requested_rate {
        pcm_f32
    } else {
        resample(&pcm_f32, requested_rate, device_rate)
    };
    // Upmix to N channels by duplicating the mono buffer across each
    // output channel. Speakers expect interleaved samples.
    let interleaved = if device_channels <= 1 {
        resampled
    } else {
        upmix_mono_to_channels(&resampled, device_channels)
    };

    let config = cpal::StreamConfig {
        channels: device_channels,
        sample_rate: cpal::SampleRate(device_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let buffer = Arc::new(interleaved);
    let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cursor_for_cb = Arc::clone(&cursor);
    let buffer_for_cb = Arc::clone(&buffer);
    let done_for_cb = Arc::clone(&done);

    let err_cb = |e: cpal::StreamError| {
        tracing::warn!("wylde-voice: playback cpal stream error: {e}");
    };

    let stream = match sample_format {
        SampleFormat::F32 => device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    write_chunk_f32(out, &buffer_for_cb, &cursor_for_cb, &done_for_cb);
                },
                err_cb,
                None,
            )
            .map_err(|e| PlaybackError::Build(e.to_string()))?,
        SampleFormat::I16 => device
            .build_output_stream(
                &config,
                move |out: &mut [i16], _info: &cpal::OutputCallbackInfo| {
                    write_chunk_i16(out, &buffer_for_cb, &cursor_for_cb, &done_for_cb);
                },
                err_cb,
                None,
            )
            .map_err(|e| PlaybackError::Build(e.to_string()))?,
        SampleFormat::U16 => device
            .build_output_stream(
                &config,
                move |out: &mut [u16], _info: &cpal::OutputCallbackInfo| {
                    write_chunk_u16(out, &buffer_for_cb, &cursor_for_cb, &done_for_cb);
                },
                err_cb,
                None,
            )
            .map_err(|e| PlaybackError::Build(e.to_string()))?,
        other => {
            return Err(PlaybackError::Build(format!(
                "unsupported cpal sample format: {other:?}"
            )));
        }
    };

    stream.play().map_err(|e| PlaybackError::Play(e.to_string()))?;
    tracing::info!(
        "wylde-voice: playback running on {:?} ({} Hz, {} ch, fmt {:?}) — {} input samples @ {} Hz",
        device.name().ok(),
        device_rate,
        device_channels,
        sample_format,
        total_samples,
        requested_rate,
    );

    // Poll the done flag with a timeout proportional to the audio length
    // (audio_seconds * 1.5 + 1s headroom) — guards against a stuck device.
    let buffer_seconds = buffer.len() as f32 / (device_rate.max(1) as f32 * device_channels.max(1) as f32);
    let timeout = Duration::from_secs_f32(buffer_seconds * 1.5 + 1.0);
    let deadline = std::time::Instant::now() + timeout;
    while !done.load(Ordering::SeqCst) {
        if std::time::Instant::now() >= deadline {
            drop(stream);
            return Err(PlaybackError::Timeout(timeout));
        }
        thread::sleep(Duration::from_millis(10));
    }
    drop(stream);
    Ok(())
}

fn write_chunk_f32(
    out: &mut [f32],
    buffer: &Arc<Vec<f32>>,
    cursor: &Arc<std::sync::atomic::AtomicUsize>,
    done: &Arc<AtomicBool>,
) {
    let pos = cursor.load(Ordering::Relaxed);
    let remaining = buffer.len().saturating_sub(pos);
    let take = remaining.min(out.len());
    if take > 0 {
        out[..take].copy_from_slice(&buffer[pos..pos + take]);
        cursor.store(pos + take, Ordering::Relaxed);
    }
    if take < out.len() {
        for s in &mut out[take..] {
            *s = 0.0;
        }
    }
    if pos + take >= buffer.len() {
        done.store(true, Ordering::SeqCst);
    }
}

fn write_chunk_i16(
    out: &mut [i16],
    buffer: &Arc<Vec<f32>>,
    cursor: &Arc<std::sync::atomic::AtomicUsize>,
    done: &Arc<AtomicBool>,
) {
    let pos = cursor.load(Ordering::Relaxed);
    let remaining = buffer.len().saturating_sub(pos);
    let take = remaining.min(out.len());
    for (i, dst) in out.iter_mut().enumerate().take(take) {
        let clamped = buffer[pos + i].clamp(-1.0, 1.0);
        *dst = (clamped * i16::MAX as f32) as i16;
    }
    if take < out.len() {
        for s in &mut out[take..] {
            *s = 0;
        }
    }
    cursor.store(pos + take, Ordering::Relaxed);
    if pos + take >= buffer.len() {
        done.store(true, Ordering::SeqCst);
    }
}

fn write_chunk_u16(
    out: &mut [u16],
    buffer: &Arc<Vec<f32>>,
    cursor: &Arc<std::sync::atomic::AtomicUsize>,
    done: &Arc<AtomicBool>,
) {
    let pos = cursor.load(Ordering::Relaxed);
    let remaining = buffer.len().saturating_sub(pos);
    let take = remaining.min(out.len());
    for (i, dst) in out.iter_mut().enumerate().take(take) {
        let clamped = buffer[pos + i].clamp(-1.0, 1.0);
        // u16 zero = i16::MAX (centred). Mirrors mic.rs's u16 ingest.
        let scaled = (clamped * i16::MAX as f32) as i32 + i16::MAX as i32;
        *dst = scaled.clamp(0, u16::MAX as i32) as u16;
    }
    if take < out.len() {
        for s in &mut out[take..] {
            *s = i16::MAX as u16;
        }
    }
    cursor.store(pos + take, Ordering::Relaxed);
    if pos + take >= buffer.len() {
        done.store(true, Ordering::SeqCst);
    }
}

fn resample(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = (samples.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let lo = src_pos.floor() as usize;
        let hi = (lo + 1).min(samples.len().saturating_sub(1));
        let t = (src_pos - lo as f64) as f32;
        out.push(samples[lo] * (1.0 - t) + samples[hi] * t);
    }
    out
}

fn upmix_mono_to_channels(mono: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    let mut out = Vec::with_capacity(mono.len() * channels);
    for &s in mono {
        for _ in 0..channels {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upmix_doubles_for_stereo() {
        let mono = vec![0.1, 0.2, 0.3];
        let out = upmix_mono_to_channels(&mono, 2);
        assert_eq!(out, vec![0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn upmix_mono_passthrough() {
        let mono = vec![0.5, -0.5];
        let out = upmix_mono_to_channels(&mono, 1);
        assert_eq!(out, mono);
    }

    #[test]
    fn resample_passthrough_at_same_rate() {
        let v = vec![0.1, 0.2, 0.3];
        let out = resample(&v, 16_000, 16_000);
        assert_eq!(out, v);
    }

    #[test]
    fn resample_upsamples_when_dst_higher() {
        // 16 kHz → 48 kHz triples the sample count.
        let v: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let out = resample(&v, 16_000, 48_000);
        assert!(out.len() >= 47 && out.len() <= 48, "got {}", out.len());
    }

    #[tokio::test]
    async fn play_blocking_rejects_empty_buffer() {
        let err = play_blocking(vec![], 24_000).await.unwrap_err();
        assert!(matches!(err, PlaybackError::EmptyBuffer));
    }

    #[tokio::test]
    async fn play_blocking_rejects_zero_rate() {
        let err = play_blocking(vec![0; 100], 0).await.unwrap_err();
        assert!(matches!(err, PlaybackError::Build(_)));
    }

    #[tokio::test]
    async fn play_blocking_rejects_oversize_buffer() {
        // 24 kHz × 90 s = 2.16 M samples > 60s cap.
        let buf = vec![0_i16; 24_000 * 90];
        let err = play_blocking(buf, 24_000).await.unwrap_err();
        assert!(matches!(err, PlaybackError::BufferTooLarge(_, _)));
    }

    #[test]
    fn write_chunk_f32_pads_remainder_with_silence() {
        let buffer = Arc::new(vec![0.5_f32, 0.5, 0.5]);
        let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let mut out = vec![0.0_f32; 6];
        write_chunk_f32(&mut out, &buffer, &cursor, &done);
        assert_eq!(out[..3], [0.5, 0.5, 0.5]);
        assert_eq!(out[3..], [0.0, 0.0, 0.0]);
        assert!(done.load(Ordering::SeqCst));
    }

    /// Real-speaker integration test. Plays a short tone and asserts the
    /// call returns within a reasonable window. Marked `#[ignore]` so CI
    /// (headless, no audio device) doesn't run it.
    ///
    /// ```text
    /// cargo test -p wylde-voice playback::tests::live_default_output_completes \
    ///     -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires a working default output device"]
    async fn live_default_output_completes() {
        // 0.25 s of a 440 Hz sine at 24 kHz.
        let sample_rate = 24_000_u32;
        let duration_s = 0.25_f32;
        let n = (sample_rate as f32 * duration_s) as usize;
        let mut buf = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
            buf.push((sample * i16::MAX as f32) as i16);
        }
        let start = std::time::Instant::now();
        play_blocking(buf, sample_rate).await.expect("playback ok");
        // Should finish at roughly real-time (between 0.2 and 5 s).
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5),
            "playback elapsed {:?} out of expected range",
            elapsed,
        );
    }
}
