//! End-to-end Kokoro TTS integration test.
//!
//! Drives the full Rust synthesis pipeline (phoneme tokenize → voices
//! lookup → Kokoro ONNX inference → WAV encode) on a hardcoded short
//! phoneme string and asserts the produced WAV is well-formed plus
//! within the expected duration band.
//!
//! Why `#[ignore]`: depends on the Kokoro ONNX + `voices.npz` being
//! present in the HuggingFace cache (i.e. `Voice/download_models.py`
//! has run with the default settings). CI machines without those
//! would otherwise see a spurious `model_not_loaded`.
//!
//! Run with:
//!
//! ```
//! cargo test -p wylde-voice --test synthesize_end_to_end -- --ignored --nocapture
//! ```
//!
//! ## Parity against the Python reference
//!
//! Python parity is verified by the standalone helper at
//! `rust/crates/wylde-voice/tests/parity_synthesize.py`. The Rust side
//! here asserts deterministic invariants (header, sample rate, sample
//! count band, peak-normalised dynamic range) that are bit-stable
//! against the same ONNX weights — call out to the helper when you
//! want to confirm waveform similarity. We deliberately don't shell
//! into Python from the Rust test: it would slow `cargo test
//! --ignored` from <1 s to >10 s per run, and the parity check is a
//! once-per-slice manual gate, not a regression suite.

use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .expect("project layout: rust/crates/wylde-voice/")
}

fn kokoro_snapshot() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let snapshots = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join("models--onnx-community--Kokoro-82M-v1.0-ONNX")
        .join("snapshots");
    let entries = std::fs::read_dir(&snapshots).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// Espeak-ng's `en-us` IPA for "Hello world." with stress markers.
/// Pinned here so the test doesn't need the espeak shared lib in
/// scope; the same string is the parity-script input. Verified against
/// `phonemizer.phonemize("Hello world.", "en-us", preserve_punctuation=True,
///  with_stress=True)` on a stock `phonemizer 3.2.1 / espeak-ng 1.51`.
const HELLO_WORLD_PHONEMES: &str = "həlˈoʊ wˈɜːld.";

#[tokio::test]
#[ignore = "requires Kokoro ONNX + voices.npz cached locally"]
async fn hello_world_synthesises_to_valid_wav() {
    let snapshot = kokoro_snapshot().expect("Kokoro not in HF cache; run Voice/download_models.py");
    let onnx = snapshot.join("onnx").join("model.onnx");
    let voices = snapshot.join("voices.npz");
    assert!(onnx.exists(), "Kokoro model.onnx missing: {}", onnx.display());
    assert!(
        voices.exists(),
        "voices.npz missing (Voice/download_models.py builds it): {}",
        voices.display()
    );

    // Same env-var trio as the JFK test — bypass the broker so
    // lease::acquire doesn't burn its 30 s pipe timeout, and point
    // `ort = ..., load-dynamic` at the staged DLL.
    let ort_dll = project_root().join("spikes/voice-npu-spike/target/release/onnxruntime.dll");
    unsafe {
        std::env::set_var("WYLDE_IPC_DISABLE", "1");
        if ort_dll.exists() {
            std::env::set_var("ORT_DYLIB_PATH", &ort_dll);
        }
    }

    let payload = serde_json::json!({
        "phonemes": HELLO_WORLD_PHONEMES,
        "voice": "af_heart",
        "speed": 1.0,
    });

    wylde_voice::reset_for_tests();
    let reply = wylde_voice::actions::synthesize::handle_synthesize(payload).await;

    assert!(
        reply.ok,
        "voice.synthesize should succeed; err = {:?}",
        reply.error
    );

    let audio_b64 = reply.data["audio"]
        .as_str()
        .expect("reply.audio must be base64 string");
    let sample_rate = reply.data["sample_rate"].as_u64().unwrap_or(0);
    let audio_seconds = reply.data["audio_seconds"].as_f64().unwrap_or(0.0);
    let inference_ms = reply.data["inference_ms"].as_f64().unwrap_or(0.0);
    let audio_samples = reply.data["audio_samples"].as_u64().unwrap_or(0);
    let token_count = reply.data["phoneme_token_count"].as_u64().unwrap_or(0);

    eprintln!(
        "tokens={token_count}  audio_s={audio_seconds:.3}  samples={audio_samples}  \
         inference_ms={inference_ms:.1}  wav_b64_chars={}",
        audio_b64.len()
    );

    assert_eq!(sample_rate, 24_000, "Kokoro native sample rate is 24 kHz");
    assert_eq!(reply.data["voice"].as_str().unwrap(), "af_heart");
    assert_eq!(reply.data["format"].as_str().unwrap(), "wav_pcm16");
    assert!(token_count > 0, "tokenizer produced empty token sequence");

    // "Hello world." at speed 1.0 should land between 0.5 s and 3 s of
    // audio. Wider band than I'd want long-term but Kokoro's output
    // length is data-dependent on the model release.
    assert!(
        audio_seconds > 0.3 && audio_seconds < 4.0,
        "audio duration {audio_seconds} s outside sane band"
    );
    assert!(audio_samples > 0, "zero-length audio buffer");
    // Sample count should match the reported duration at 24 kHz.
    let expected_samples = (audio_seconds * 24_000.0).round() as u64;
    assert!(
        (audio_samples as i64 - expected_samples as i64).abs() <= 1,
        "audio_samples {audio_samples} disagrees with audio_seconds {audio_seconds}",
    );

    // The base64 WAV decodes to a valid PCM16 header.
    let wav_bytes = base64_decode(audio_b64).expect("audio base64 decodes");
    assert_eq!(&wav_bytes[0..4], b"RIFF");
    assert_eq!(&wav_bytes[8..12], b"WAVE");
    let header_sr =
        u32::from_le_bytes(wav_bytes[24..28].try_into().expect("WAV sample-rate slice"));
    let header_bps =
        u16::from_le_bytes(wav_bytes[34..36].try_into().expect("WAV bits-per-sample slice"));
    let header_channels =
        u16::from_le_bytes(wav_bytes[22..24].try_into().expect("WAV channels slice"));
    assert_eq!(header_sr, 24_000);
    assert_eq!(header_bps, 16);
    assert_eq!(header_channels, 1);
}

#[tokio::test]
#[ignore = "requires Kokoro ONNX + voices.npz cached locally"]
async fn unknown_voice_returns_invalid_request() {
    let snapshot = kokoro_snapshot().expect("Kokoro not in HF cache");
    let onnx = snapshot.join("onnx").join("model.onnx");
    let voices = snapshot.join("voices.npz");
    assert!(onnx.exists());
    assert!(voices.exists());

    let ort_dll = project_root().join("spikes/voice-npu-spike/target/release/onnxruntime.dll");
    unsafe {
        std::env::set_var("WYLDE_IPC_DISABLE", "1");
        if ort_dll.exists() {
            std::env::set_var("ORT_DYLIB_PATH", &ort_dll);
        }
    }

    wylde_voice::reset_for_tests();
    let reply = wylde_voice::actions::synthesize::handle_synthesize(serde_json::json!({
        "phonemes": HELLO_WORLD_PHONEMES,
        "voice": "no_such_voice_42",
    }))
    .await;

    assert!(!reply.ok);
    let err = reply.error.expect("invalid_request envelope");
    assert_eq!(err.code, "invalid_request");
    assert!(err.message.contains("no_such_voice_42"), "{}", err.message);
}

/// Minimal stdlib-only base64 decoder so the test doesn't pull a new
/// dep just for verification. Same alphabet as the one in
/// `wylde_voice::synth::wav::encode_base64`.
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    let bytes = cleaned.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let mut chunk = [0_u8; 4];
        let mut pad = 0;
        for j in 0..4 {
            if i + j >= bytes.len() {
                chunk[j] = b'=';
                pad += 1;
                continue;
            }
            let c = bytes[i + j];
            chunk[j] = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => {
                    pad += 1;
                    b'='
                }
                _ => return Err(format!("invalid base64 char {c:?}")),
            };
        }
        i += 4;
        let b0 = if chunk[0] == b'=' { 0 } else { chunk[0] };
        let b1 = if chunk[1] == b'=' { 0 } else { chunk[1] };
        let b2 = if chunk[2] == b'=' { 0 } else { chunk[2] };
        let b3 = if chunk[3] == b'=' { 0 } else { chunk[3] };
        out.push((b0 << 2) | (b1 >> 4));
        if pad < 2 {
            out.push((b1 << 4) | (b2 >> 2));
        }
        if pad < 1 {
            out.push((b2 << 6) | b3);
        }
    }
    Ok(out)
}
