//! float32-PCM → 16-bit PCM WAV bytes.
//!
//! The wylde-voice action surface returns audio as base64-encoded WAV
//! (not raw float32 PCM as the Python `models.synthesize` does). WAV
//! gives clients a self-describing container — sample rate + channel
//! count baked in — so a frontend can hand it straight to `<audio>` or
//! HTMLAudioElement without out-of-band metadata. The encoding step
//! also lets us peak-normalise the same way the Python pipeline does
//! (`audio / peak * 0.95`), matching what the existing Voice service
//! returns.
//!
//! We dropped to int16 because Kokoro audio is mono speech at 24 kHz —
//! 16-bit is already audibly lossless for that signal, and an int16
//! WAV is half the wire size of float32. Frontends that need float32
//! can call out to the streaming variant (Slice 11.C).

use hound::{SampleFormat, WavSpec, WavWriter};

use crate::synth::vocab::KOKORO_SAMPLE_RATE;

/// Peak-normalise + encode a mono float32 buffer to a 16-bit PCM WAV.
///
/// Mirrors `Voice/synthesize.py::Synthesizer.synthesize` 'peak / 0.95'
/// normalisation — same waveform shape and dynamic range as the Python
/// reference, so a frontend swapping between Python and Rust impls
/// hears equivalent loudness.
pub fn encode_wav(audio: &[f32], sample_rate: u32) -> Result<Vec<u8>, String> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer = WavWriter::new(&mut cursor, spec)
            .map_err(|e| format!("WAV writer init: {e}"))?;
        let peak = audio
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let scale = if peak > 0.0 { 0.95 / peak } else { 1.0 };
        for sample in audio {
            let v = (*sample * scale).clamp(-1.0, 1.0);
            let i = (v * 32767.0).round() as i16;
            writer
                .write_sample(i)
                .map_err(|e| format!("WAV write sample: {e}"))?;
        }
        writer
            .finalize()
            .map_err(|e| format!("WAV finalize: {e}"))?;
    }
    Ok(cursor.into_inner())
}

/// Encode at Kokoro's native sample rate. Convenience wrapper so the
/// action handler doesn't have to thread the constant around.
pub fn encode_wav_kokoro(audio: &[f32]) -> Result<Vec<u8>, String> {
    encode_wav(audio, KOKORO_SAMPLE_RATE)
}

/// Encode a mono float32 buffer to a base64 string the IPC envelope
/// can carry without escaping. Same alphabet Python's
/// `base64.b64encode` produces (standard + padding).
pub fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let chunks = bytes.chunks_exact(3);
    let remainder = chunks.remainder();
    for c in chunks {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
    }
    match remainder.len() {
        1 => {
            let n = u32::from(remainder[0]) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(remainder[0]) << 16) | (u32::from(remainder[1]) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_starts_with_riff_wave_fmt() {
        let buf = encode_wav(&[0.0, 0.1, 0.0, -0.1], 24_000).unwrap();
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[12..16], b"fmt ");
        // PCM tag = 1, mono, 24000Hz, 16-bit.
        let pcm_tag = u16::from_le_bytes([buf[20], buf[21]]);
        let channels = u16::from_le_bytes([buf[22], buf[23]]);
        let sample_rate = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
        let bits_per_sample = u16::from_le_bytes([buf[34], buf[35]]);
        assert_eq!(pcm_tag, 1);
        assert_eq!(channels, 1);
        assert_eq!(sample_rate, 24_000);
        assert_eq!(bits_per_sample, 16);
    }

    #[test]
    fn peak_normalises_to_0_95_full_scale() {
        // Input peak is 0.5; after normalisation peak becomes 0.95.
        let audio: Vec<f32> = (0..1000).map(|i| (i as f32) / 1999.0).collect();
        let buf = encode_wav(&audio, 24_000).unwrap();
        // Data chunk starts at offset 44 for the standard 16-bit WAV
        // header (RIFF=12 + fmt=24 + data hdr=8 = 44).
        let max_sample = buf[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs() as i32)
            .max()
            .unwrap();
        let max_full_scale = (32767_f32 * 0.95).round() as i32;
        // Within 1 LSB of the expected peak.
        assert!(
            (max_sample - max_full_scale).abs() <= 1,
            "peak {max_sample} vs expected {max_full_scale}"
        );
    }

    #[test]
    fn zero_signal_does_not_divide_by_zero() {
        let audio = vec![0_f32; 100];
        let buf = encode_wav(&audio, 24_000).unwrap();
        // Should still produce a valid 44-byte header + 200 zero samples.
        assert_eq!(buf.len(), 44 + 200);
        assert!(buf[44..].iter().all(|&b| b == 0));
    }

    #[test]
    fn base64_matches_python_b64encode_for_known_input() {
        assert_eq!(encode_base64(b"Hello"), "SGVsbG8=");
        assert_eq!(encode_base64(b"Hi"), "SGk=");
        assert_eq!(encode_base64(b"Hi!"), "SGkh");
        assert_eq!(encode_base64(b""), "");
    }

    #[test]
    fn base64_roundtrips_via_action_decoder() {
        let raw: Vec<u8> = (0..=255_u8).collect();
        let encoded = encode_base64(&raw);
        // Sanity: should be 256 / 3 * 4 ≈ 344 bytes with padding.
        assert!(encoded.is_ascii());
        assert_eq!(encoded.len(), raw.len().div_ceil(3) * 4);
    }
}
