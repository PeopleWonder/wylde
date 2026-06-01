//! WAV decoding for `voice.transcribe`.
//!
//! Decodes a 16-bit-PCM WAV file (mono or stereo) into a `Vec<f32>`
//! of mono 16 kHz samples in `[-1.0, 1.0]`. Stereo collapses to mono
//! by averaging the channels; non-16k rates resample with a naive
//! linear interpolator. The Python predecessor uses
//! `Voice/audio_io.py`'s `sounddevice`-driven capture pipeline which
//! always emits at 16 kHz; for the transcribe action we accept any
//! sample rate and normalise here so callers don't have to.

use std::io::Cursor;

use thiserror::Error;

/// Sample rate Whisper expects — non-negotiable; the model was trained
/// on 16 kHz audio and the mel filterbank assumes it.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("wav decode failed: {0}")]
    WavDecode(#[from] hound::Error),

    #[error("unsupported sample format: only 16-bit PCM is accepted, got {bits}-bit {format}")]
    UnsupportedFormat { bits: u16, format: String },

    #[error("empty audio: no samples in WAV body")]
    Empty,
}

/// Decode a WAV byte buffer into mono 16 kHz f32 samples.
///
/// Steps: parse the header → collapse to mono → resample to 16 kHz.
/// Output is normalised so peak |s| ≤ 1.0 (mirrors
/// `Voice/transcribe.py`'s peak-normalise step before handing audio
/// to faster-whisper).
pub fn decode_wav(bytes: &[u8]) -> Result<Vec<f32>, AudioError> {
    let reader = hound::WavReader::new(Cursor::new(bytes))?;
    let spec = reader.spec();

    if spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
        return Err(AudioError::UnsupportedFormat {
            bits: spec.bits_per_sample,
            format: format!("{:?}", spec.sample_format),
        });
    }

    let channels = spec.channels.max(1) as usize;
    let mut samples: Vec<f32> = Vec::with_capacity(reader.len() as usize / channels.max(1));

    if channels == 1 {
        for s in reader.into_samples::<i16>() {
            let s = s? as f32 / i16::MAX as f32;
            samples.push(s);
        }
    } else {
        // Multi-channel → mono by averaging across channels per frame.
        let mut buf = Vec::with_capacity(channels);
        for s in reader.into_samples::<i16>() {
            buf.push(s? as f32 / i16::MAX as f32);
            if buf.len() == channels {
                let mean: f32 = buf.iter().sum::<f32>() / channels as f32;
                samples.push(mean);
                buf.clear();
            }
        }
    }

    if samples.is_empty() {
        return Err(AudioError::Empty);
    }

    let resampled = if spec.sample_rate == WHISPER_SAMPLE_RATE {
        samples
    } else {
        linear_resample(&samples, spec.sample_rate, WHISPER_SAMPLE_RATE)
    };

    let peak = resampled.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
    let out = if peak > 0.0 {
        resampled.into_iter().map(|s| s / peak).collect()
    } else {
        resampled
    };

    Ok(out)
}

/// Naive linear-interpolation resampler. Good enough for the encoder-
/// path proof (we hand the same-rate test corpus through it); a real
/// production transcribe action that handles arbitrary-rate input
/// should switch to a windowed-sinc or polyphase resampler (`rubato`
/// crate). Tracked in the Slice 11.A+ punchlist.
fn linear_resample(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let lo = src_pos.floor() as usize;
        let hi = (lo + 1).min(input.len() - 1);
        let t = (src_pos - lo as f64) as f32;
        let s = input[lo] * (1.0 - t) + input[hi] * t;
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let mut writer = hound::WavWriter::new(Cursor::new(&mut buf), spec).unwrap();
            for s in samples {
                writer.write_sample(*s).unwrap();
            }
            writer.finalize().unwrap();
        }
        buf
    }

    #[test]
    fn decodes_mono_16k_passthrough() {
        let wav = synthetic_wav(16_000, 1, &[0, i16::MAX / 2, -i16::MAX / 2, 0]);
        let pcm = decode_wav(&wav).unwrap();
        assert_eq!(pcm.len(), 4);
        // After peak-normalisation, the maxabs sample should be ~1.0.
        let peak = pcm.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!((peak - 1.0).abs() < 1e-3);
    }

    #[test]
    fn collapses_stereo_to_mono() {
        // Two frames, L/R: (max, -max), (0, 0) → frame averages: 0, 0.
        let wav = synthetic_wav(16_000, 2, &[i16::MAX, -i16::MAX, 0, 0]);
        let pcm = decode_wav(&wav).unwrap();
        assert_eq!(pcm.len(), 2);
    }

    #[test]
    fn resamples_when_rate_differs() {
        // 32 kHz → 16 kHz halves the sample count.
        let mut input: Vec<i16> = Vec::with_capacity(32);
        for i in 0..32 {
            input.push(((i as f32 / 32.0) * 10_000.0) as i16);
        }
        let wav = synthetic_wav(32_000, 1, &input);
        let pcm = decode_wav(&wav).unwrap();
        // Linear resampler rounds up via ceil(); allow ±1 vs the exact
        // 16-sample target.
        assert!(pcm.len() <= 17 && pcm.len() >= 15, "got {}", pcm.len());
    }

    #[test]
    fn rejects_empty_wav() {
        let wav = synthetic_wav(16_000, 1, &[]);
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioError::Empty));
    }

    #[test]
    fn rejects_24bit_wav() {
        // hound rejects this on the spec line before our check kicks in;
        // assert we surface the error cleanly either way.
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let mut writer = hound::WavWriter::new(Cursor::new(&mut buf), spec).unwrap();
            writer.write_sample(0_i32).unwrap();
            writer.finalize().unwrap();
        }
        let err = decode_wav(&buf).unwrap_err();
        // Either WavDecode or UnsupportedFormat — both are acceptable
        // "we won't try to decode this" signals.
        assert!(matches!(
            err,
            AudioError::WavDecode(_) | AudioError::UnsupportedFormat { .. }
        ));
    }
}
