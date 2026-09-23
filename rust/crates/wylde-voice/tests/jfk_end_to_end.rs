//! End-to-end Whisper STT integration test.
//!
//! Drives the full pipeline (WAV decode → log-mel → encoder → decoder
//! loop → tokenizer decode) on the bundled `jfk.wav` clip and asserts
//! the produced transcript contains the expected JFK quote words.
//!
//! Why `#[ignore]`: depends on the Whisper ONNX model + tokenizer
//! being present in the HuggingFace cache. CI machines without the
//! model would otherwise see a spurious `model_not_loaded`.
//!
//! Run with:
//!
//! ```
//! cargo test -p wylde-voice --test jfk_end_to_end -- --ignored --nocapture
//! ```
//!
//! The test pins to `onnx-community/whisper-tiny.en` — the same model
//! the voice-npu-spike used and the only Whisper variant the spike
//! validated end-to-end.

use std::path::PathBuf;

fn project_root() -> PathBuf {
    // The test file lives at:
    //   rust/crates/wylde-voice/tests/jfk_end_to_end.rs
    // CARGO_MANIFEST_DIR points at rust/crates/wylde-voice/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .and_then(|p| p.parent()) // rust/
        .map(|p| p.to_path_buf())
        .expect("project layout: rust/crates/wylde-voice/")
}

fn jfk_wav_path() -> PathBuf {
    project_root().join("spikes/voice-npu-spike/jfk.wav")
}

fn whisper_tiny_en_snapshot() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let snapshots = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join("models--onnx-community--whisper-tiny.en")
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

#[tokio::test]
#[ignore = "requires whisper-tiny.en model files cached locally"]
async fn jfk_wav_transcribes_to_expected_quote() {
    let wav = jfk_wav_path();
    assert!(wav.exists(), "jfk.wav fixture missing at {}", wav.display());

    let snapshot = whisper_tiny_en_snapshot().expect("whisper-tiny.en not in HF cache");
    let enc = snapshot.join("onnx").join("encoder_model.onnx");
    let dec = snapshot.join("onnx").join("decoder_model.onnx");
    let tok = snapshot.join("tokenizer.json");
    assert!(enc.exists(), "encoder ONNX missing: {}", enc.display());
    assert!(dec.exists(), "decoder ONNX missing: {}", dec.display());
    assert!(tok.exists(), "tokenizer.json missing: {}", tok.display());

    // SAFETY: tests are single-threaded by default; we set these before
    // any other thread reads them.
    //   * WYLDE_IPC_DISABLE skips the broker round-trip — there's no
    //     broker in the test environment, so lease::acquire would
    //     otherwise eat the 30 s pipe timeout once per encoder/decoder.
    //   * ORT_DYLIB_PATH points at the onnxruntime.dll the spike
    //     pre-staged. `ort = "..., load-dynamic"` opens the DLL at
    //     first session-build; without ORT_DYLIB_PATH the symbol
    //     resolution silently looks in PATH only and blocks forever
    //     on some Windows setups.
    let ort_dll = project_root().join("spikes/voice-npu-spike/target/release/onnxruntime.dll");
    unsafe {
        std::env::set_var("WYLDE_VOICE_STT_MODEL", "onnx-community/whisper-tiny.en");
        std::env::set_var("WYLDE_VOICE_STT_ENCODER_PATH", &enc);
        std::env::set_var("WYLDE_IPC_DISABLE", "1");
        if ort_dll.exists() {
            std::env::set_var("ORT_DYLIB_PATH", &ort_dll);
        }
    }

    let payload = serde_json::json!({
        "audio_path": wav.to_string_lossy(),
        "language": "en",
        "max_new_tokens": 96,
    });

    wylde_voice::reset_for_tests();
    let reply = wylde_voice::actions::transcribe::handle_transcribe(payload).await;

    assert!(
        reply.ok,
        "voice.transcribe should succeed; err = {:?}",
        reply.error
    );

    let text = reply.data["text"]
        .as_str()
        .expect("reply.text must be a string")
        .to_lowercase();
    eprintln!("transcript = {text:?}");
    eprintln!(
        "audio_s = {:.3}  enc_ms = {:.1}  dec_ms = {:.1}  tokens = {}",
        reply.data["audio_seconds"].as_f64().unwrap_or(0.0),
        reply.data["encoder_inference_ms"].as_f64().unwrap_or(0.0),
        reply.data["decoder_inference_ms"].as_f64().unwrap_or(0.0),
        reply.data["token_count"].as_u64().unwrap_or(0),
    );

    // JFK clip is the opening line of his inaugural address:
    // "And so my fellow Americans, ask not what your country can do for you;
    //  ask what you can do for your country."
    // We assert on a few high-confidence content words.
    let must_contain = ["country", "ask"];
    for w in must_contain {
        assert!(
            text.contains(w),
            "transcript missing expected word {w:?}; got {text:?}"
        );
    }
    assert!(
        !text.contains("<|"),
        "special token markers leaked into transcript: {text:?}"
    );
    assert_eq!(reply.data["language"].as_str().unwrap(), "en");
}
