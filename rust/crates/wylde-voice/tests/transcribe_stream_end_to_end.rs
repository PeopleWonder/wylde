//! End-to-end streaming-transcribe integration test (Slice 11.C).
//!
//! Drives `voice.transcribe_stream` against the bundled JFK WAV sample
//! and asserts the streamed chunk sequence shape: one `encoder_complete`,
//! N `token` chunks with monotonic indices + accumulating delta length,
//! one final `transcript_complete` whose `text` matches the concatenated
//! deltas.
//!
//! Why `#[ignore]`: requires the whisper-tiny.en ONNX bundle + tokenizer
//! to be present in the HuggingFace cache (i.e. first-run model download
//! has happened) AND a JFK sample WAV at the expected path. CI machines
//! without that disk state would see a spurious `model_not_loaded`.
//!
//! Run with:
//!
//! ```
//! cargo test -p wylde-voice --test transcribe_stream_end_to_end \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::time::Instant;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .expect("project layout: rust/crates/wylde-voice/")
}

fn jfk_wav() -> Option<PathBuf> {
    // Same fixture the unary jfk_end_to_end test pins — the spike's
    // copy is the canonical one on disk. (Other Voice/tests/ and
    // data/voice/ paths are kept as fallbacks for when the spike dir
    // is checked out separately.)
    let candidates = [
        project_root().join("spikes/voice-npu-spike/jfk.wav"),
        project_root().join("Voice").join("tests").join("jfk.wav"),
        project_root().join("data").join("voice").join("jfk.wav"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

fn whisper_snapshot_present() -> bool {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) else {
        return false;
    };
    PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join("models--onnx-community--whisper-tiny.en")
        .join("snapshots")
        .read_dir()
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "requires whisper-tiny.en ONNX + JFK WAV cached locally"]
async fn transcribe_stream_emits_encoder_then_tokens_then_complete() {
    let Some(wav) = jfk_wav() else {
        eprintln!("skipping: JFK WAV fixture not present");
        return;
    };
    if !whisper_snapshot_present() {
        eprintln!("skipping: whisper-tiny.en not in HF cache");
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

    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    let payload = serde_json::json!({
        "audio_path": wav.to_string_lossy(),
        "language": "en",
        "max_new_tokens": 96,
    });
    let start = Instant::now();
    let handler = tokio::spawn(async move {
        wylde_voice::actions::transcribe::handle_transcribe_stream(payload, tx).await;
    });

    let mut chunks: Vec<serde_json::Value> = Vec::new();
    let mut first_chunk_ms: Option<f64> = None;
    let mut first_token_ms: Option<f64> = None;
    while let Some(item) = rx.recv().await {
        let chunk = item.expect("no stream-level errors");
        let now_ms = start.elapsed().as_secs_f64() * 1000.0;
        if first_chunk_ms.is_none() {
            first_chunk_ms = Some(now_ms);
        }
        if chunk["type"] == "token" && first_token_ms.is_none() {
            first_token_ms = Some(now_ms);
        }
        chunks.push(chunk);
    }
    let end_to_end_ms = start.elapsed().as_secs_f64() * 1000.0;
    handler.await.expect("handler joins cleanly");

    assert!(
        chunks.len() >= 3,
        "expected encoder_complete + ≥1 token + transcript_complete, got {} chunks",
        chunks.len(),
    );

    // First chunk must be encoder_complete.
    assert_eq!(chunks[0]["type"], "encoder_complete");
    assert!(chunks[0]["encoder_inference_ms"].as_f64().unwrap() > 0.0);
    assert!(chunks[0]["audio_seconds"].as_f64().unwrap() > 0.0);

    // Last chunk must be transcript_complete.
    let last = chunks.last().unwrap();
    assert_eq!(last["type"], "transcript_complete");
    let final_text = last["text"].as_str().expect("text present").to_owned();
    assert!(
        !final_text.is_empty(),
        "transcript_complete carried empty text",
    );

    // Token chunks: indices monotonically increasing from 0.
    let token_chunks: Vec<&serde_json::Value> = chunks
        .iter()
        .filter(|c| c["type"] == "token")
        .collect();
    assert!(!token_chunks.is_empty(), "no token chunks streamed");
    for (i, tok) in token_chunks.iter().enumerate() {
        assert_eq!(tok["index"].as_u64().unwrap(), i as u64);
    }

    // Concatenated deltas should reconstruct (≈) the final text.
    let concatenated: String = token_chunks
        .iter()
        .filter_map(|c| c["delta"].as_str())
        .collect();
    assert_eq!(
        concatenated.trim(),
        final_text.trim(),
        "concatenated deltas should equal final transcript",
    );
    let unary_equiv_ms = last["total_inference_ms"].as_f64().unwrap_or(0.0);
    let ttfb = first_chunk_ms.unwrap_or(0.0);
    let ttft = first_token_ms.unwrap_or(0.0);
    eprintln!(
        "streamed {} tokens; ttfb_ms={:.1}  ttft_ms={:.1}  \
         end_to_end_ms={:.1}  unary_equiv_ms={:.1}  transcript={:?}",
        token_chunks.len(),
        ttfb,
        ttft,
        end_to_end_ms,
        unary_equiv_ms,
        final_text,
    );
}
