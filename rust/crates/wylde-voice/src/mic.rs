//! Microphone capture (Slice 11.D).
//!
//! Opens the default input device via `cpal`, downmixes to mono, naive-
//! linear-resamples to 16 kHz, and emits fixed-length `i16` chunks to
//! every active subscriber. The Python predecessor (`Voice/audio_io.py`
//! and `Voice/record.py`) used `sounddevice`; the Rust port stays in the
//! same shape — int16 PCM mono at 16 kHz — so the byte-for-byte
//! downstream contract with `voice.transcribe` is preserved.
//!
//! ## Why a dedicated worker thread
//!
//! `cpal::Stream` is `!Send` + `!Sync` on every backend (WASAPI on
//! Windows, CoreAudio on macOS, ALSA on Linux). The audio driver
//! requires that the thread that built the stream also drops it. The
//! Rust async action handlers run inside Tokio's threadpool, so the
//! capture thread is a dedicated `std::thread::spawn` — it owns the
//! `Stream`, the driver callback fires on the audio-driver thread,
//! and chunks flow out via a `tokio::sync::broadcast` so any number of
//! `voice.mic.chunks` subscribers can fan-out the same PCM.
//!
//! ## Backpressure
//!
//! Broadcast subscribers that fall behind get `RecvError::Lagged` —
//! the wake-word listener and the streaming `voice.mic.chunks` handler
//! both treat lag as a non-fatal warning (drop chunks and continue),
//! same drop-oldest semantics the Python `MicrophoneStream` queue has
//! in `Voice/record.py`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use thiserror::Error;
use tokio::sync::broadcast;

/// Sample rate every downstream consumer (Whisper, openWakeWord) expects.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Wake-word frame size — 80 ms at 16 kHz. openWakeWord ingests audio
/// in exactly this granularity per its melspectrogram model export.
pub const WAKEWORD_FRAME_SAMPLES: usize = 1_280;

/// Default chunk size for stand-alone `voice.mic.chunks` subscribers —
/// 50 ms at 16 kHz, the same granularity `Voice/audio_io.py` used.
pub const DEFAULT_MIC_CHUNK_SAMPLES: usize = 800;

/// Broadcast buffer depth. Each subscriber sees up to this many chunks
/// of lag before it starts getting `RecvError::Lagged`. 32 chunks ≈
/// 1.6 s of 50 ms frames, which is more than the streaming handler
/// should ever fall behind.
const BROADCAST_BUFFER: usize = 32;

#[derive(Debug, Error)]
pub enum MicError {
    #[error("no default input device available")]
    NoDevice,

    #[error("input device has no supported config: {0}")]
    NoSupportedConfig(String),

    #[error("cpal stream build failed: {0}")]
    Build(String),

    #[error("cpal stream play failed: {0}")]
    Play(String),
}

/// Owning handle for an active capture session. Dropping the handle
/// signals the worker thread to stop and `drop()`s the underlying
/// `cpal::Stream`. The worker thread joins synchronously on drop —
/// cpal streams that don't get dropped on their build thread leak
/// driver resources on Windows.
pub struct MicCapture {
    chunks: broadcast::Sender<Arc<Vec<i16>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    chunk_samples: usize,
    actual_input_sample_rate: u32,
    actual_input_channels: u16,
}

impl MicCapture {
    /// Open the default input device and start emitting `chunk_samples`-
    /// sized i16 mono frames at 16 kHz on the returned broadcast.
    pub fn start(chunk_samples: usize) -> Result<Self, MicError> {
        if chunk_samples == 0 {
            return Err(MicError::Build("chunk_samples must be > 0".to_owned()));
        }

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or(MicError::NoDevice)?;
        let supported = device
            .default_input_config()
            .map_err(|e| MicError::NoSupportedConfig(e.to_string()))?;

        let sample_format = supported.sample_format();
        let input_sample_rate = supported.sample_rate().0;
        let input_channels = supported.channels();

        let (chunks_tx, _) = broadcast::channel::<Arc<Vec<i16>>>(BROADCAST_BUFFER);
        let stop = Arc::new(AtomicBool::new(false));

        let chunks_for_worker = chunks_tx.clone();
        let stop_for_worker = Arc::clone(&stop);

        let worker = thread::Builder::new()
            .name("wylde-voice-mic".to_owned())
            .spawn(move || {
                if let Err(e) = run_capture_thread(
                    device,
                    sample_format,
                    input_sample_rate,
                    input_channels,
                    chunk_samples,
                    chunks_for_worker,
                    stop_for_worker,
                ) {
                    tracing::error!("wylde-voice: mic capture thread failed: {e}");
                }
            })
            .map_err(|e| MicError::Build(format!("spawn worker thread: {e}")))?;

        Ok(Self {
            chunks: chunks_tx,
            stop,
            worker: Some(worker),
            chunk_samples,
            actual_input_sample_rate: input_sample_rate,
            actual_input_channels: input_channels,
        })
    }

    /// Subscribe to the chunk stream. Each subscriber gets its own
    /// receiver — broadcast semantics, no consumption across
    /// subscribers.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Vec<i16>>> {
        self.chunks.subscribe()
    }

    pub fn chunk_samples(&self) -> usize {
        self.chunk_samples
    }

    pub fn input_sample_rate(&self) -> u32 {
        self.actual_input_sample_rate
    }

    pub fn input_channels(&self) -> u16 {
        self.actual_input_channels
    }

    pub fn target_sample_rate(&self) -> u32 {
        TARGET_SAMPLE_RATE
    }

    /// True once the worker thread has signalled it has exited and the
    /// stream is gone. Used by tests + the action layer's `voice.mic.stop`
    /// reply to confirm shutdown.
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) && self.worker.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Signal the worker to stop and join. Idempotent.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            if let Err(e) = handle.join() {
                tracing::warn!("wylde-voice: mic worker join panicked: {e:?}");
            }
        }
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Enumerate the host's input device names plus the system default's
/// name (Slice 6 — the Settings → Voice mic-device picker).
///
/// Read-only host query: it does **not** open a stream, build a capture,
/// or touch the [`MicCapture`] singleton, so it's safe to call while a
/// capture is live. Names are de-duplicated and sorted for a stable
/// picker order. The default may be `None` when the host reports no
/// default input device (a headless box); the list may legitimately be
/// empty in that case too.
pub fn list_input_device_names() -> Result<(Option<String>, Vec<String>), MicError> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let mut names = Vec::new();
    let devices = host
        .input_devices()
        .map_err(|e| MicError::NoSupportedConfig(e.to_string()))?;
    for device in devices {
        if let Ok(name) = device.name() {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    Ok((default_name, names))
}

fn run_capture_thread(
    device: cpal::Device,
    sample_format: SampleFormat,
    input_sample_rate: u32,
    input_channels: u16,
    chunk_samples: usize,
    chunks_tx: broadcast::Sender<Arc<Vec<i16>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), MicError> {
    let config = cpal::StreamConfig {
        channels: input_channels,
        sample_rate: cpal::SampleRate(input_sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let mut accumulator: Vec<f32> = Vec::with_capacity(chunk_samples * 4);

    let make_err = |e: cpal::StreamError| {
        tracing::warn!("wylde-voice: cpal stream error: {e}");
    };

    let stream = match sample_format {
        SampleFormat::F32 => device
            .build_input_stream(
                &config,
                {
                    let chunks_tx = chunks_tx.clone();
                    move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                        ingest_samples(
                            data,
                            input_channels,
                            input_sample_rate,
                            chunk_samples,
                            &mut accumulator,
                            &chunks_tx,
                        );
                    }
                },
                make_err,
                None,
            )
            .map_err(|e| MicError::Build(e.to_string()))?,
        SampleFormat::I16 => device
            .build_input_stream(
                &config,
                {
                    let chunks_tx = chunks_tx.clone();
                    move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                        let buf: Vec<f32> =
                            data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                        ingest_samples(
                            &buf,
                            input_channels,
                            input_sample_rate,
                            chunk_samples,
                            &mut accumulator,
                            &chunks_tx,
                        );
                    }
                },
                make_err,
                None,
            )
            .map_err(|e| MicError::Build(e.to_string()))?,
        SampleFormat::U16 => device
            .build_input_stream(
                &config,
                {
                    let chunks_tx = chunks_tx.clone();
                    move |data: &[u16], _info: &cpal::InputCallbackInfo| {
                        let buf: Vec<f32> = data
                            .iter()
                            .map(|&s| (s as f32 - i16::MAX as f32) / i16::MAX as f32)
                            .collect();
                        ingest_samples(
                            &buf,
                            input_channels,
                            input_sample_rate,
                            chunk_samples,
                            &mut accumulator,
                            &chunks_tx,
                        );
                    }
                },
                make_err,
                None,
            )
            .map_err(|e| MicError::Build(e.to_string()))?,
        other => {
            return Err(MicError::Build(format!(
                "unsupported cpal sample format: {other:?}"
            )));
        }
    };

    stream.play().map_err(|e| MicError::Play(e.to_string()))?;
    tracing::info!(
        "wylde-voice: mic capture running on {:?} ({} Hz, {} ch, fmt {:?}) → 16 kHz mono i16 \
         in {}-sample chunks",
        device.name().ok(),
        input_sample_rate,
        input_channels,
        sample_format,
        chunk_samples,
    );

    while !stop.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_millis(50));
    }

    drop(stream);
    Ok(())
}

/// Convert one cpal callback's worth of interleaved samples into
/// 16 kHz mono i16 and push as many `chunk_samples`-sized broadcasts
/// out as the running accumulator will yield. Pure function over the
/// callback's locals; no shared state besides the broadcast Sender.
fn ingest_samples(
    interleaved: &[f32],
    channels: u16,
    src_sample_rate: u32,
    chunk_samples: usize,
    accumulator: &mut Vec<f32>,
    chunks_tx: &broadcast::Sender<Arc<Vec<i16>>>,
) {
    let mono = downmix_to_mono(interleaved, channels);
    let resampled = resample_to_16k(&mono, src_sample_rate);
    accumulator.extend_from_slice(&resampled);
    while accumulator.len() >= chunk_samples {
        let drained: Vec<i16> = accumulator
            .drain(..chunk_samples)
            .map(float_to_i16)
            .collect();
        // Best-effort fan-out — broadcast::send returns Err only when
        // there are no active subscribers, which is the steady state
        // until somebody opens voice.mic.chunks or starts a wake-word
        // listener. Dropping the chunk is correct in that case.
        let _ = chunks_tx.send(Arc::new(drained)); // wylde-check: discard-result-ok
    }
}

fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    if channels == 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for frame in 0..frames {
        let base = frame * channels;
        let mut sum = 0.0_f32;
        for c in 0..channels {
            sum += interleaved[base + c];
        }
        out.push(sum / channels as f32);
    }
    out
}

fn resample_to_16k(samples: &[f32], src_sample_rate: u32) -> Vec<f32> {
    if src_sample_rate == TARGET_SAMPLE_RATE || samples.is_empty() {
        return samples.to_vec();
    }
    // Naive linear resampler — same shape as
    // `transcribe::audio::linear_resample`. Acceptable for wake-word
    // and short-utterance STT (high-frequency aliasing is benign at
    // this scale); a future swap to `rubato`/polyphase tracked in the
    // 11.E punchlist.
    let ratio = src_sample_rate as f64 / TARGET_SAMPLE_RATE as f64;
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

fn float_to_i16(s: f32) -> i16 {
    let clamped = s.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_input_device_names_is_well_formed() {
        // Doesn't open a stream — safe in CI. On a box with no audio
        // host it may Err or return an empty list; either is acceptable.
        // The contract under test is "no panic, sorted+deduped names".
        if let Ok((_default, names)) = list_input_device_names() {
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "names must be sorted");
            let mut deduped = names.clone();
            deduped.dedup();
            assert_eq!(names.len(), deduped.len(), "names must be de-duplicated");
        }
    }

    #[test]
    fn downmix_mono_passthrough() {
        let v = vec![0.1, -0.2, 0.3];
        let out = downmix_to_mono(&v, 1);
        assert_eq!(out, v);
    }

    #[test]
    fn downmix_stereo_averages_channels() {
        let v = vec![1.0, -1.0, 0.4, 0.6];
        let out = downmix_to_mono(&v, 2);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn resample_passthrough_at_target_rate() {
        let v = vec![0.1, 0.2, 0.3];
        let out = resample_to_16k(&v, TARGET_SAMPLE_RATE);
        assert_eq!(out, v);
    }

    #[test]
    fn resample_halves_when_doubled_rate() {
        // 32 kHz → 16 kHz halves the sample count.
        let v: Vec<f32> = (0..32).map(|i| i as f32 / 32.0).collect();
        let out = resample_to_16k(&v, 32_000);
        assert!(out.len() == 16 || out.len() == 15, "got {}", out.len());
    }

    #[test]
    fn float_to_i16_clamps_outside_range() {
        assert_eq!(float_to_i16(0.0), 0);
        assert_eq!(float_to_i16(1.0), i16::MAX);
        assert_eq!(float_to_i16(2.0), i16::MAX);
        assert_eq!(float_to_i16(-2.0), -i16::MAX);
    }

    #[test]
    fn ingest_emits_when_accumulator_fills() {
        let (tx, mut rx) = broadcast::channel::<Arc<Vec<i16>>>(8);
        let mut acc: Vec<f32> = Vec::new();
        // 16 kHz, mono, 1280 samples: feed 1300 — expect one chunk
        // emitted, 20 leftover in the accumulator.
        let buf: Vec<f32> = (0..1_300).map(|i| (i as f32 / 1_300.0) - 0.5).collect();
        ingest_samples(&buf, 1, TARGET_SAMPLE_RATE, 1_280, &mut acc, &tx);
        let chunk = rx.try_recv().expect("one chunk emitted");
        assert_eq!(chunk.len(), 1_280);
        assert_eq!(acc.len(), 20);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ingest_drops_silently_when_no_subscribers() {
        // No receivers — broadcast::send returns Err but ingest_samples
        // must not panic. Drains the accumulator anyway.
        let (tx, _) = broadcast::channel::<Arc<Vec<i16>>>(4);
        let mut acc: Vec<f32> = Vec::new();
        let buf: Vec<f32> = vec![0.5; 2_000];
        ingest_samples(&buf, 1, TARGET_SAMPLE_RATE, 1_280, &mut acc, &tx);
        // 2000 samples / 1280-chunk = one full chunk; 720 leftover.
        assert_eq!(acc.len(), 720);
    }

    /// Real-mic integration test. Opens the default input device for
    /// one second and asserts that at least one chunk arrived. Marked
    /// `#[ignore]` because CI machines have no working input device.
    /// Run locally with:
    ///
    /// ```
    /// cargo test -p wylde-voice mic::tests::live_default_input_emits_chunks \
    ///     -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a working default input device"]
    fn live_default_input_emits_chunks() {
        let cap = MicCapture::start(WAKEWORD_FRAME_SAMPLES).expect("default input available");
        let mut rx = cap.subscribe();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut received_any = false;
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.try_recv() {
                assert_eq!(chunk.len(), WAKEWORD_FRAME_SAMPLES);
                received_any = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        cap.stop();
        assert!(received_any, "no mic chunks arrived in 2 s");
    }
}
