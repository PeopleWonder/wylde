//! Voice activity detection (Slice 3 — `docs/plans/voice-rust-port.md`).
//!
//! Pure-Rust port of the Python recorder's VAD
//! ([`Voice/record.py`](../../../../Voice/record.py)): an energy-floor +
//! zero-crossing-rate (ZCR) speech detector ([`Vad`]) plus the
//! silence-triggered capture state machine ([`VadGate`]) that
//! `record_until_silence` ran. No ONNX, no FFI, no model download — the
//! detector is a few arithmetic ops per chunk, matching the Python
//! implementation byte-for-byte in shape (and value, modulo i16↔f32
//! normalisation) so turn-taking behaviour is unchanged after the
//! strangler-fig cutover.
//!
//! ## Why energy + ZCR (and not WebRTC / Silero)
//!
//! The plan offered three candidates: `webrtc-vad` (a C binding, fails the
//! everything-Rust rule), Silero via `ort` (needs a model download +
//! adds an ONNX session to the hot path), and a pure-Rust energy detector.
//! The Python service already shipped the energy+ZCR detector and tuned
//! its constants against the Wylde user's mic + HVAC noise floor, so porting it
//! 1:1 is both the lowest-risk parity move and the only pure-Rust option.
//!
//! ## Detector
//!
//! [`Vad::is_speech`] tracks an asymmetric IIR energy floor (rises fast
//! toward louder backgrounds, falls slowly during quiet gaps so brief
//! gusts don't read as speech) and gates on ZCR to reject broadband noise
//! (fans/HVAC sit at ZCR > 0.35; voiced speech at ~0.05–0.15). A logistic
//! maps the RMS-over-floor ratio to a speech probability compared against
//! [`VadConfig::threshold`].
//!
//! ## Gate
//!
//! [`VadGate`] is the stateful "record until silence" loop: it folds
//! mid-word pauses back into the speech segment and ends the utterance
//! only after [`VadConfig::silence_timeout_ms`] of continuous silence
//! *once speech has started*. The caller (the orchestrator's capture
//! adapter) drives it one chunk at a time and reads back the accumulated
//! speech plus a start/end span for diagnostics.

// ── Detector tuning constants (ported verbatim from `Voice/record.py`) ──

/// IIR floor rise rate. ~1.6 s time constant at 512-sample/16 kHz chunks;
/// how fast the floor climbs toward a louder background.
const FLOOR_ALPHA: f32 = 0.02;

/// IIR floor fall rate. Slow, so the floor stays near the HVAC average and
/// brief quiet gaps inside a word don't reset it.
const FLOOR_BETA: f32 = 0.005;

/// ZCR gate. Amplitude-independent (sign-based). Voiced speech < 0.15,
/// unvoiced fricatives < 0.30, broadband noise > 0.35.
const ZCR_MAX: f32 = 0.35;

/// Initial energy floor before any audio is seen. Matches Python's
/// `_Vad._energy_floor = 0.001` in the normalised [-1, 1] amplitude scale.
const INITIAL_ENERGY_FLOOR: f32 = 0.001;

// ── Config defaults (mirror `Voice/config.py::VadConfig` YAML defaults) ──

/// Default speech-probability threshold. Python `vad.threshold` default.
pub const DEFAULT_THRESHOLD: f32 = 0.65;

/// Default trailing silence before an utterance is considered finished.
/// Python `vad.silence_timeout_ms` default.
pub const DEFAULT_SILENCE_TIMEOUT_MS: u32 = 1_800;

/// Tunables for the detector + gate. Built from [`crate::config::Config`]
/// (env-driven) at the capture call site.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    /// Speech probability in `[0, 1]` at/above which a chunk counts as
    /// speech.
    pub threshold: f32,
    /// Continuous silence (ms) after speech has started that ends the
    /// utterance.
    pub silence_timeout_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            silence_timeout_ms: DEFAULT_SILENCE_TIMEOUT_MS,
        }
    }
}

/// Stateful energy-floor + ZCR speech detector. One instance per
/// recording — the floor adapts across the call.
#[derive(Debug, Clone)]
pub struct Vad {
    threshold: f32,
    energy_floor: f32,
}

impl Vad {
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            energy_floor: INITIAL_ENERGY_FLOOR,
        }
    }

    /// Classify one chunk of 16-bit PCM as speech or not. Mirrors
    /// `Voice/record.py::_Vad.is_speech`: ZCR reject first (cheap), then
    /// adapt the floor and threshold the logistic speech probability.
    pub fn is_speech(&mut self, chunk: &[i16]) -> bool {
        if chunk.is_empty() {
            return false;
        }
        if zcr(chunk) > ZCR_MAX {
            return false;
        }
        let rms = rms_normalised(chunk);
        // Asymmetric IIR: fast rise (ALPHA) when the chunk is louder than
        // the floor, slow fall (BETA) when it's quieter.
        if rms < self.energy_floor {
            self.energy_floor += (rms - self.energy_floor) * FLOOR_BETA;
        } else {
            self.energy_floor += (rms - self.energy_floor) * FLOOR_ALPHA;
        }
        let floor = self.energy_floor.max(1e-7);
        let prob = 1.0 / (1.0 + (-1.5 * (rms / floor - 3.0)).exp());
        prob >= self.threshold
    }

    /// Current adapted energy floor — exposed for tests / diagnostics.
    pub fn energy_floor(&self) -> f32 {
        self.energy_floor
    }
}

/// Zero-crossing rate of a chunk, in `[0, ~1]`. Amplitude-independent: it
/// counts sign changes between consecutive samples. `i16::signum` returns
/// `-1/0/1` exactly like `numpy.sign`, and peak-normalisation (which
/// Python applies first) can't change a sample's sign, so we operate on
/// the raw integer signs — bit-identical to the Python computation.
fn zcr(chunk: &[i16]) -> f32 {
    if chunk.len() < 2 || chunk.iter().all(|&s| s == 0) {
        return 0.0;
    }
    let mut sum_abs_diff: i32 = 0;
    for w in chunk.windows(2) {
        sum_abs_diff += (i32::from(w[1].signum()) - i32::from(w[0].signum())).abs();
    }
    // Python divides by `2 * len` (full length, not len-1) — match it.
    sum_abs_diff as f32 / (2.0 * chunk.len() as f32)
}

/// RMS of a chunk, normalised to the `[-1, 1]` amplitude scale the Python
/// detector worked in (sounddevice handed it float32). Dividing each i16
/// by `i16::MAX` puts the energy floor (`0.001`) on the same footing.
fn rms_normalised(chunk: &[i16]) -> f32 {
    if chunk.is_empty() {
        return 0.0;
    }
    let scale = f64::from(i16::MAX);
    let sum_sq: f64 = chunk
        .iter()
        .map(|&s| {
            let x = f64::from(s) / scale;
            x * x
        })
        .sum();
    (sum_sq / chunk.len() as f64).sqrt() as f32
}

/// Outcome of feeding one chunk to a [`VadGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Keep capturing — either still waiting for speech to start, or
    /// inside an utterance / a short pause.
    Continue,
    /// Speech started and has now been followed by
    /// `silence_timeout_ms` of continuous silence. The utterance is done;
    /// stop capturing and read [`VadGate::into_speech`].
    SpeechEnded,
}

/// Silence-triggered capture state machine — the [`Vad`] plus the
/// accumulation logic from `Voice/record.py::record_until_silence`.
///
/// Drive it one chunk at a time with [`observe`](VadGate::observe). Speech
/// chunks accumulate; a run of silence shorter than the timeout is held in
/// a pending buffer and folded back in if speech resumes (mid-word pause),
/// or dropped as trailing silence once the utterance ends.
#[derive(Debug, Clone)]
pub struct VadGate {
    vad: Vad,
    sample_rate: u32,
    silence_timeout_s: f32,

    speech: Vec<i16>,
    pending_silence: Vec<i16>,
    speech_started: bool,
    silence_s: f32,

    // Diagnostics: sample offsets (from the start of capture) of the first
    // detected speech and the end of the last speech chunk.
    observed: usize,
    speech_start_sample: Option<usize>,
    speech_end_sample: usize,
}

impl VadGate {
    pub fn new(cfg: &VadConfig, sample_rate: u32) -> Self {
        Self {
            vad: Vad::new(cfg.threshold),
            sample_rate: sample_rate.max(1),
            silence_timeout_s: cfg.silence_timeout_ms as f32 / 1_000.0,
            speech: Vec::new(),
            pending_silence: Vec::new(),
            speech_started: false,
            silence_s: 0.0,
            observed: 0,
            speech_start_sample: None,
            speech_end_sample: 0,
        }
    }

    /// Feed one capture chunk. Returns [`GateDecision::SpeechEnded`] once
    /// the trailing-silence timeout trips after speech has begun.
    pub fn observe(&mut self, chunk: &[i16]) -> GateDecision {
        let chunk_start = self.observed;
        self.observed += chunk.len();
        let chunk_dur_s = chunk.len() as f32 / self.sample_rate as f32;

        if self.vad.is_speech(chunk) {
            if !self.speech_started {
                self.speech_started = true;
                self.speech_start_sample = Some(chunk_start);
            }
            // A pause shorter than the timeout gets folded back into the
            // utterance so we don't clip mid-word breaths.
            self.speech.append(&mut self.pending_silence);
            self.silence_s = 0.0;
            self.speech.extend_from_slice(chunk);
            self.speech_end_sample = self.observed;
        } else if self.speech_started {
            self.pending_silence.extend_from_slice(chunk);
            self.silence_s += chunk_dur_s;
            if self.silence_s >= self.silence_timeout_s {
                return GateDecision::SpeechEnded;
            }
        }
        GateDecision::Continue
    }

    /// True once at least one speech chunk has been seen.
    pub fn speech_started(&self) -> bool {
        self.speech_started
    }

    /// Number of accumulated speech samples (excludes trailing silence).
    pub fn speech_len(&self) -> usize {
        self.speech.len()
    }

    /// Start/end of the speech segment in milliseconds from capture start.
    /// `None` if no speech was ever detected.
    pub fn speech_span_ms(&self) -> Option<(u64, u64)> {
        let start = self.speech_start_sample?;
        let to_ms = |samples: usize| (samples as u64 * 1_000) / self.sample_rate as u64;
        Some((to_ms(start), to_ms(self.speech_end_sample)))
    }

    /// Consume the gate and return the captured speech segment (trailing
    /// silence dropped — matches Python, which never appends the final
    /// `pending_silence`).
    pub fn into_speech(self) -> Vec<i16> {
        self.speech
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: u32 = 16_000;

    /// One 50 ms chunk (800 samples @ 16 kHz) of pure silence.
    fn silence_chunk() -> Vec<i16> {
        vec![0_i16; 800]
    }

    /// One 50 ms chunk of a `freq`-Hz sine at the given peak amplitude
    /// (0.0–1.0). Low-frequency tones have low ZCR + high RMS → voiced.
    fn tone_chunk(freq: f32, amp: f32) -> Vec<i16> {
        (0..800)
            .map(|n| {
                let t = n as f32 / SR as f32;
                let v = (TAU * freq * t).sin() * amp;
                (v * i16::MAX as f32) as i16
            })
            .collect()
    }

    /// Alternating ±full-scale — maximal ZCR, the broadband-noise shape
    /// the ZCR gate exists to reject.
    fn broadband_chunk() -> Vec<i16> {
        (0..800)
            .map(|n| if n % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect()
    }

    #[test]
    fn zcr_silence_is_zero() {
        assert_eq!(zcr(&silence_chunk()), 0.0);
    }

    #[test]
    fn zcr_broadband_is_high() {
        // Sign flips every sample → ZCR approaches 1.0, well over the gate.
        assert!(zcr(&broadband_chunk()) > ZCR_MAX);
    }

    #[test]
    fn zcr_low_freq_tone_is_low() {
        // A 200 Hz tone crosses zero ~400×/s → ZCR ≈ 0.025 at 16 kHz.
        assert!(zcr(&tone_chunk(200.0, 0.5)) < 0.15);
    }

    #[test]
    fn rms_silence_is_zero() {
        assert_eq!(rms_normalised(&silence_chunk()), 0.0);
    }

    #[test]
    fn rms_tone_matches_amplitude() {
        // RMS of a full-amplitude sine is amp / sqrt(2) ≈ 0.354 at amp=0.5.
        let r = rms_normalised(&tone_chunk(200.0, 0.5));
        assert!((r - 0.5 / 2.0_f32.sqrt()).abs() < 0.02, "rms={r}");
    }

    #[test]
    fn silence_is_not_speech() {
        let mut vad = Vad::new(DEFAULT_THRESHOLD);
        for _ in 0..10 {
            assert!(!vad.is_speech(&silence_chunk()));
        }
    }

    #[test]
    fn broadband_noise_is_rejected_by_zcr() {
        let mut vad = Vad::new(DEFAULT_THRESHOLD);
        // Loud, but high ZCR → must be rejected as non-speech.
        assert!(!vad.is_speech(&broadband_chunk()));
    }

    #[test]
    fn loud_low_freq_tone_is_speech() {
        let mut vad = Vad::new(DEFAULT_THRESHOLD);
        // First chunk already clears the floor (init 0.001, tone RMS ~0.35).
        assert!(vad.is_speech(&tone_chunk(200.0, 0.5)));
    }

    #[test]
    fn empty_chunk_is_not_speech() {
        let mut vad = Vad::new(DEFAULT_THRESHOLD);
        assert!(!vad.is_speech(&[]));
    }

    #[test]
    fn gate_pure_silence_never_starts_and_yields_nothing() {
        let cfg = VadConfig::default();
        let mut gate = VadGate::new(&cfg, SR);
        for _ in 0..100 {
            assert_eq!(gate.observe(&silence_chunk()), GateDecision::Continue);
        }
        assert!(!gate.speech_started());
        assert!(gate.speech_span_ms().is_none());
        assert!(gate.into_speech().is_empty());
    }

    #[test]
    fn gate_speech_then_silence_ends_utterance() {
        let cfg = VadConfig::default(); // 1800 ms silence timeout
        let mut gate = VadGate::new(&cfg, SR);

        // ~0.5 s of speech: 10 × 50 ms tone chunks.
        for _ in 0..10 {
            assert_eq!(gate.observe(&tone_chunk(200.0, 0.5)), GateDecision::Continue);
        }
        assert!(gate.speech_started());

        // Silence chunks: 1800 ms / 50 ms = 36 chunks to trip the timeout.
        // The first 35 hold (pending), the 36th ends the utterance.
        let mut ended_at = None;
        for i in 0..40 {
            if gate.observe(&silence_chunk()) == GateDecision::SpeechEnded {
                ended_at = Some(i);
                break;
            }
        }
        let ended_at = ended_at.expect("utterance should end after silence timeout");
        // 1800ms / 50ms ≈ 36 silent chunks; f32 accumulation of 0.05s can
        // land the trip on the 36th or 37th chunk (index 35 or 36).
        assert!(
            ended_at == 35 || ended_at == 36,
            "expected ~36 silent chunks to end the utterance, got index {ended_at}"
        );

        // Captured speech is the ~0.5 s segment, trailing silence dropped.
        let span = gate.speech_span_ms().expect("speech span recorded");
        assert_eq!(span.0, 0, "speech started at t=0");
        assert!(span.1 >= 480 && span.1 <= 520, "speech ends ~500ms, got {}", span.1);
        let speech = gate.into_speech();
        assert_eq!(speech.len(), 10 * 800, "10 chunks of 800 samples");
    }

    #[test]
    fn gate_folds_short_midword_pause_back_in() {
        let cfg = VadConfig {
            threshold: DEFAULT_THRESHOLD,
            silence_timeout_ms: 1_800,
        };
        let mut gate = VadGate::new(&cfg, SR);

        // speech … short pause (well under 1800ms) … speech.
        for _ in 0..5 {
            gate.observe(&tone_chunk(200.0, 0.5));
        }
        // 4 × 50 ms = 200 ms pause — under the timeout, gets folded in.
        for _ in 0..4 {
            assert_eq!(gate.observe(&silence_chunk()), GateDecision::Continue);
        }
        for _ in 0..5 {
            gate.observe(&tone_chunk(200.0, 0.5));
        }

        // The 200 ms pause is now part of the speech segment:
        // (5 + 4 + 5) × 800 samples.
        assert_eq!(gate.speech_len(), 14 * 800);
    }

    #[test]
    fn gate_span_ms_uses_sample_rate() {
        let cfg = VadConfig::default();
        let mut gate = VadGate::new(&cfg, SR);
        // Lead with 4 silent chunks (200 ms) before speech starts.
        for _ in 0..4 {
            gate.observe(&silence_chunk());
        }
        gate.observe(&tone_chunk(200.0, 0.5));
        let (start_ms, end_ms) = gate.speech_span_ms().unwrap();
        assert_eq!(start_ms, 200, "speech starts after 200ms of lead silence");
        assert_eq!(end_ms, 250, "one 50ms speech chunk ends at 250ms");
    }
}
