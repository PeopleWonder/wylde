//! End-to-end streaming-synthesize integration test (Slice 11.C).
//!
//! Drives `voice.synthesize_stream` against a hardcoded multi-sentence
//! IPA phoneme string and asserts the streamed chunk sequence shape: one
//! `synthesize_start`, N `audio_chunk` frames matching the sentence
//! count, one final `synthesize_complete` whose totals match the per-
//! chunk fields summed. Each `audio_chunk`'s WAV must be a valid 24 kHz
//! 16-bit PCM RIFF file.
//!
//! Why `#[ignore]`: depends on the Kokoro ONNX + `voices.npz` being
//! present in the HF cache (i.e. `Voice/download_models.py` has run).
//!
//! Run with:
//!
//! ```
//! cargo test -p wylde-voice --test synthesize_stream_end_to_end \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "windows")]

use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .expect("project layout: rust/crates/wylde-voice/")
}

fn kokoro_snapshot_present() -> bool {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) else {
        return false;
    };
    let root = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join("models--onnx-community--Kokoro-82M-v1.0-ONNX")
        .join("snapshots");
    let Ok(mut entries) = root.read_dir() else {
        return false;
    };
    entries.any(|e| {
        e.ok()
            .map(|e| {
                let p = e.path();
                p.is_dir()
                    && p.join("voices.npz").exists()
                    && (p.join("onnx").join("model.onnx").exists() || p.join("model.onnx").exists())
            })
            .unwrap_or(false)
    })
}

// Three sentences of phonemes — "Hello world. How are you? I am fine."
// espeak-ng en-us output, with stress + preserve_punctuation=True.
const MULTI_SENTENCE_PHONEMES: &str =
    "həlˈoʊ wˈɜːld. hˈaʊ ɑːɹ jˈuː? aɪ ˈæm fˈaɪn.";

#[tokio::test]
#[ignore = "requires Kokoro ONNX + voices.npz cached locally"]
async fn synthesize_stream_emits_start_chunks_then_complete() {
    if !kokoro_snapshot_present() {
        eprintln!("skipping: Kokoro snapshot not in HF cache");
        return;
    }

    let ort_dll = project_root().join("spikes/voice-npu-spike/target/release/onnxruntime.dll");
    unsafe {
        std::env::set_var("WYLDE_IPC_DISABLE", "1");
        if ort_dll.exists() {
            std::env::set_var("ORT_DYLIB_PATH", &ort_dll);
        }
    }

    wylde_voice::reset_for_tests();

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let payload = serde_json::json!({
        "phonemes": MULTI_SENTENCE_PHONEMES,
        "voice": "af_heart",
        "speed": 1.0,
    });
    let handler = tokio::spawn(async move {
        wylde_voice::actions::synthesize::handle_synthesize_stream(payload, tx).await;
    });

    let mut chunks: Vec<serde_json::Value> = Vec::new();
    while let Some(item) = rx.recv().await {
        chunks.push(item.expect("no stream-level errors"));
    }
    handler.await.expect("handler joins cleanly");

    assert!(chunks.len() >= 3, "got only {} chunks", chunks.len());

    // First chunk: synthesize_start.
    let start = &chunks[0];
    assert_eq!(start["type"], "synthesize_start");
    let chunk_count = start["chunk_count"].as_u64().expect("chunk_count present");
    assert_eq!(chunk_count, 3, "three sentences → three chunks");
    assert_eq!(start["sample_rate"].as_u64().unwrap(), 24_000);
    assert_eq!(start["voice"].as_str().unwrap(), "af_heart");

    // Middle chunks: audio_chunk × chunk_count, contiguous indices.
    let audio_chunks: Vec<&serde_json::Value> = chunks
        .iter()
        .filter(|c| c["type"] == "audio_chunk")
        .collect();
    assert_eq!(audio_chunks.len() as u64, chunk_count);
    let mut summed_samples: u64 = 0;
    let mut summed_inference_ms: f64 = 0.0;
    for (i, c) in audio_chunks.iter().enumerate() {
        assert_eq!(c["index"].as_u64().unwrap(), i as u64);
        let samples = c["audio_samples"].as_u64().unwrap();
        assert!(samples > 0, "chunk {i} produced zero samples");
        summed_samples += samples;
        summed_inference_ms += c["inference_ms"].as_f64().unwrap();
        let b64 = c["audio"].as_str().unwrap();
        let wav = base64_decode(b64).expect("base64 decodes");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let sr = u32::from_le_bytes(wav[24..28].try_into().unwrap());
        assert_eq!(sr, 24_000);
    }

    // Last chunk: synthesize_complete; totals match the sum.
    let last = chunks.last().unwrap();
    assert_eq!(last["type"], "synthesize_complete");
    assert_eq!(last["chunk_count"].as_u64().unwrap(), chunk_count);
    let total_samples = last["total_audio_samples"].as_u64().unwrap();
    assert_eq!(
        total_samples, summed_samples,
        "synthesize_complete.total_audio_samples must equal sum of per-chunk audio_samples",
    );
    let total_ms = last["total_inference_ms"].as_f64().unwrap();
    assert!(
        (total_ms - summed_inference_ms).abs() < 1.0,
        "synthesize_complete.total_inference_ms {total_ms} should ~match summed {summed_inference_ms}",
    );

    eprintln!(
        "streamed {} chunks; total {} samples, {:.1} ms inference",
        chunk_count, total_samples, total_ms,
    );
}

/// Minimal stdlib-only base64 decoder — same alphabet as the WAV encoder
/// in `wylde_voice::synth::wav::encode_base64`. Mirrors the helper in
/// `synthesize_end_to_end.rs` so the streaming test doesn't drag in a
/// new dep.
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
