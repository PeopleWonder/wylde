//! `voice.synthesize` — text *or* phoneme string → WAV bytes (Kokoro TTS).
//!
//! ## Scope
//!
//! End-to-end TTS: accept either plain English `text` (phonemised in
//! pure Rust via [`crate::synth::g2p`], Slice 2) or an explicit
//! `phonemes` string, tokenise it, slice the per-length style row out of
//! `voices.npz`, run one Kokoro ONNX inference pass, peak-normalise,
//! pack into a 16-bit PCM WAV. Reply mirrors [`crate::actions::transcribe`]:
//! latency + device + audio metadata fields.
//!
//! ## Slice 2 — text path (G2P) landed
//!
//! Slice 11.B shipped the deterministic *phoneme* half only; the
//! `text → phonemes` step ran Python-side (`models.synthesize` →
//! `Voice/synthesize.py` → `kokoro_onnx` → espeak-ng). Slice 2 ports
//! that step to Rust ([`crate::synth::g2p`], built on `misaki-rs` — the
//! Rust port of Kokoro's own *misaki* phonemiser), so this action now
//! speaks arbitrary assistant text without any Python dependency. A
//! `phonemes` field is still honoured (and takes precedence) for direct
//! callers and parity tests.
//!
//! ## What this action still does NOT do
//!
//! * Auto-load voices that aren't in the snapshot's `voices.npz`.
//!   `Voice/download_models.py` builds the bundle; that has to have
//!   run before first-call.
//!
//! Slice 11.C adds [`handle_synthesize_stream`] (the
//! `voice.synthesize_stream` streaming variant) — chunks phonemes at
//! sentence boundaries and emits one base64 WAV per chunk so the
//! caller can start playback before the full utterance is rendered.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::actions::error::{
    inference_failed, invalid_request, model_not_loaded,
};
use crate::config::Config;
use crate::lease;
use crate::state;
use crate::synth::kokoro::{KokoroInferError, KokoroLoadError, KokoroSynth};
use crate::synth::voices::{VoiceStyle, Voices, VoicesLoadError};
use crate::synth::{
    british_for_voice, encode_base64, encode_wav_kokoro, pad_with_zero, split_phonemes,
    text_to_phonemes, tokenize,
};
use crate::synth::vocab::{KOKORO_SAMPLE_RATE, MAX_PHONEME_LENGTH};

/// Default voice when the payload doesn't specify one. Matches the
/// Python service's `Voice/config.yaml` default + the wylde-voice
/// `WYLDE_VOICE_TTS_VOICE` fallback.
const DEFAULT_VOICE: &str = "af_heart";

/// Default playback speed multiplier — mirrors
/// `kokoro_onnx.Kokoro.create`'s default of 1.0.
const DEFAULT_SPEED: f32 = 1.0;

/// Resolve the voice name from the payload, falling back to the
/// configured `WYLDE_VOICE_TTS_VOICE` and then [`DEFAULT_VOICE`].
fn resolve_voice_name(payload: &Value, cfg: &Config) -> String {
    payload
        .get("voice")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| {
            if cfg.tts_voice.is_empty() {
                DEFAULT_VOICE.to_owned()
            } else {
                cfg.tts_voice.clone()
            }
        })
}

/// Resolve the (clamped) speed multiplier from the payload.
fn resolve_speed(payload: &Value) -> f32 {
    payload
        .get("speed")
        .and_then(Value::as_f64)
        .map(|s| s as f32)
        .unwrap_or(DEFAULT_SPEED)
        .clamp(0.5, 2.0)
}

/// Resolve the phoneme string a synth call should render.
///
/// Accepts either:
/// * an explicit `phonemes` string — used verbatim (the path direct
///   callers and parity tests rely on); takes precedence when present, or
/// * plain `text` — phonemised in-process via [`crate::synth::g2p`]
///   (Slice 2). `voice` selects the G2P dialect (British `b…` voices →
///   en-GB).
///
/// Returns `invalid_request` when neither is supplied (or the supplied
/// one is blank / phonemises to nothing).
fn resolve_phonemes(payload: &Value, voice: &str) -> Result<String, IpcError> {
    if let Some(raw) = payload.get("phonemes").and_then(Value::as_str) {
        if raw.trim().is_empty() {
            return Err(invalid_request(
                "payload.phonemes is required (non-empty string)",
            ));
        }
        return Ok(raw.to_owned());
    }
    if let Some(text) = payload.get("text").and_then(Value::as_str) {
        if text.trim().is_empty() {
            return Err(invalid_request(
                "payload.text is required (non-empty string)",
            ));
        }
        let phonemes = text_to_phonemes(text, british_for_voice(voice));
        if phonemes.trim().is_empty() {
            return Err(invalid_request(
                "text produced no phonemes (G2P) — nothing to synthesize",
            ));
        }
        return Ok(phonemes);
    }
    Err(invalid_request(
        "payload requires `text` or `phonemes` (non-empty string)",
    ))
}

/// `voice.synthesize` payload schema (supply `text` *or* `phonemes`):
/// ```jsonc
/// {
///   "text":      "Hello there",  // English text — phonemised via G2P (Slice 2)
///   "phonemes":  "həlˈoʊ",       // OR explicit IPA; takes precedence if both given
///   "voice":     "af_heart",     // optional, defaults to WYLDE_VOICE_TTS_VOICE
///   "speed":     1.0             // optional, defaults to 1.0 (clamped [0.5, 2.0])
/// }
/// ```
pub async fn handle_synthesize(payload: Value) -> Reply {
    let cfg = Config::get();
    let voice_name = resolve_voice_name(&payload, cfg);
    let speed = resolve_speed(&payload);

    // Slice 2: accept plain `text` (phonemised in Rust via misaki-rs) as
    // well as an explicit `phonemes` string. Dialect follows the voice.
    let phonemes = match resolve_phonemes(&payload, &voice_name) {
        Ok(p) => p,
        Err(e) => return Reply::err(e),
    };

    let token_result = tokenize(&phonemes);
    if token_result.tokens.is_empty() {
        return Reply::err(invalid_request(
            "phonemes contained no in-vocab characters after filtering — \
             nothing to synthesize",
        ));
    }
    let token_len = token_result.tokens.len();
    if token_len > MAX_PHONEME_LENGTH {
        // tokenize() truncates, so this branch is defensive — kept so a
        // future refactor doesn't silently emit out-of-range token ids.
        return Reply::err(invalid_request(format!(
            "tokenised phonemes exceed Kokoro max length {MAX_PHONEME_LENGTH}",
        )));
    }
    let input_ids = pad_with_zero(&token_result.tokens);

    let voices = match ensure_voices().await {
        Ok(v) => v,
        Err(e) => return Reply::err(e),
    };
    let voice = match voices.get(&voice_name) {
        Some(v) => v,
        None => {
            return Reply::err(invalid_request(format!(
                "voice {voice_name:?} not in voices.npz — known voices: {:?}",
                voices.names()
            )));
        }
    };
    let style_row = match voice.style_for_token_len(token_len) {
        Some(s) => s.to_vec(),
        None => {
            return Reply::err(invalid_request(format!(
                "voice {voice_name:?} style table does not cover token-length {token_len} \
                 (max {})",
                crate::synth::voices::VOICE_STYLE_LENGTHS,
            )));
        }
    };

    let kokoro = match ensure_kokoro().await {
        Ok(k) => k,
        Err(e) => return Reply::err(e),
    };

    let t0 = Instant::now();
    let audio = match kokoro.synthesize(&input_ids, &style_row, speed) {
        Ok(a) => a,
        Err(KokoroInferError::Run(msg)) => {
            return Reply::err(inference_failed(format!("kokoro.run: {msg}")));
        }
        Err(KokoroInferError::OutputShape(msg)) => {
            return Reply::err(inference_failed(format!("kokoro output: {msg}")));
        }
    };
    let inference_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let audio_seconds = audio.len() as f32 / KOKORO_SAMPLE_RATE as f32;

    let wav = match encode_wav_kokoro(&audio) {
        Ok(b) => b,
        Err(msg) => return Reply::err(inference_failed(format!("WAV encode: {msg}"))),
    };
    let audio_b64 = encode_base64(&wav);

    Reply::ok(json!({
        "audio_seconds": audio_seconds,
        "device": "CPU",
        "backend": "cpu",
        "inference_ms": inference_ms,
        "sample_rate": KOKORO_SAMPLE_RATE,
        "format": "wav_pcm16",
        "voice": voice_name,
        "speed": speed,
        "phoneme_token_count": token_len,
        "truncated": token_result.truncated,
        "audio_samples": audio.len(),
        "audio": audio_b64,
    }))
}

/// `voice.synthesize_stream` — streaming variant. Same payload schema as
/// [`handle_synthesize`] (`phonemes`, `voice?`, `speed?`). Splits the
/// phoneme string at sentence-shaped terminators
/// (see [`crate::synth::tokenizer::split_phonemes`]) and emits one
/// base64-encoded 16-bit PCM WAV chunk per sub-utterance, so playback
/// can start before the full sentence is rendered.
///
/// Chunk schema:
/// ```jsonc
/// // First chunk — emitted after voices / model are warm but before
/// // any Kokoro inference runs. Lets callers pre-allocate buffers.
/// {"type": "synthesize_start", "voice": "af_heart", "speed": 1.0,
///  "sample_rate": 24000, "chunk_count": 3, "format": "wav_pcm16",
///  "device": "CPU", "backend": "cpu"}
/// // One per phoneme chunk:
/// {"type": "audio_chunk", "index": 0, "phonemes": "həlˈoʊ.",
///  "phoneme_token_count": 6, "truncated": false,
///  "audio_seconds": 0.42, "audio_samples": 10080, "inference_ms": 18.3,
///  "audio": "<base64 WAV>"}
/// // Final summary:
/// {"type": "synthesize_complete", "voice": "af_heart", "speed": 1.0,
///  "chunk_count": 3, "total_audio_seconds": 1.27, "total_inference_ms": 54.8,
///  "total_audio_samples": 30480, "sample_rate": 24000}
/// ```
///
/// Each `audio_chunk`'s WAV is independently playable (full RIFF
/// header), so the consumer can concatenate them by skipping every
/// non-first WAV header, or just decode + queue them as separate clips.
pub async fn handle_synthesize_stream(payload: Value, sender: StreamSender) {
    let cfg = Config::get();
    let voice_name = resolve_voice_name(&payload, cfg);
    let speed = resolve_speed(&payload);

    // Slice 2: accept plain `text` (G2P) as well as explicit `phonemes`.
    let phonemes = match resolve_phonemes(&payload, &voice_name) {
        Ok(p) => p,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    let chunks = split_phonemes(&phonemes);
    if chunks.is_empty() {
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(invalid_request(
                "phonemes contained no synthesisable content (empty after splitting + trim)",
            )))
            .await;
        return;
    }
    // Materialise to owned strings so the inference task can hold them
    // across awaits without borrowing the original payload buffer.
    let chunk_strs: Vec<String> = chunks.iter().map(|s| (*s).to_owned()).collect();
    let chunk_count = chunk_strs.len();

    let voices = match ensure_voices().await {
        Ok(v) => v,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };
    if voices.get(&voice_name).is_none() {
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(invalid_request(format!(
                "voice {voice_name:?} not in voices.npz — known voices: {:?}",
                voices.names()
            ))))
            .await;
        return;
    }
    let kokoro = match ensure_kokoro().await {
        Ok(k) => k,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    if sender
        .send(Ok(json!({
            "type": "synthesize_start",
            "voice": voice_name,
            "speed": speed,
            "sample_rate": KOKORO_SAMPLE_RATE,
            "chunk_count": chunk_count,
            "format": "wav_pcm16",
            "device": "CPU",
            "backend": "cpu",
        })))
        .await
        .is_err()
    {
        return;
    }

    let mut total_samples: u64 = 0;
    let mut total_inference_ms: f64 = 0.0;

    for (index, chunk) in chunk_strs.iter().enumerate() {
        let chunk_clone = chunk.clone();
        let voice_obj = voices.get(&voice_name).expect("voice presence checked above");
        let synth_result = synthesize_one_chunk(&chunk_clone, voice_obj, Arc::clone(&kokoro), speed);
        match synth_result {
            Ok(out) => {
                total_samples += out.audio_samples as u64;
                total_inference_ms += out.inference_ms;
                if sender
                    .send(Ok(json!({
                        "type": "audio_chunk",
                        "index": index,
                        "phonemes": chunk_clone,
                        "phoneme_token_count": out.token_count,
                        "truncated": out.truncated,
                        "audio_seconds": out.audio_seconds,
                        "audio_samples": out.audio_samples,
                        "inference_ms": out.inference_ms,
                        "audio": out.audio_b64,
                    })))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(SynthChunkError::Skipped) => {
                // Tokenises to zero in-vocab characters (e.g. a `.` on
                // its own from a `..` sequence). Skip silently rather
                // than failing the whole stream — matches the lenient
                // semantics callers expect from a streaming endpoint.
                continue;
            }
            Err(SynthChunkError::Failed(err)) => {
                let _ = sender.send(Err(err)).await; // wylde-check: discard-result-ok
                return;
            }
        }
    }

    let total_audio_seconds = total_samples as f64 / KOKORO_SAMPLE_RATE as f64;
    let _ = sender // wylde-check: discard-result-ok
        .send(Ok(json!({
            "type": "synthesize_complete",
            "voice": voice_name,
            "speed": speed,
            "chunk_count": chunk_count,
            "total_audio_seconds": total_audio_seconds,
            "total_inference_ms": total_inference_ms,
            "total_audio_samples": total_samples,
            "sample_rate": KOKORO_SAMPLE_RATE,
        })))
        .await;
}

/// Per-chunk synth output for the streaming handler. Holds just the
/// fields the streaming reply needs — no internal buffers stay live
/// across the next chunk.
struct ChunkOutput {
    audio_b64: String,
    audio_seconds: f32,
    audio_samples: usize,
    inference_ms: f64,
    token_count: usize,
    truncated: bool,
}

enum SynthChunkError {
    Skipped,
    Failed(IpcError),
}

fn synthesize_one_chunk(
    phonemes: &str,
    voice: &VoiceStyle,
    kokoro: Arc<KokoroSynth>,
    speed: f32,
) -> Result<ChunkOutput, SynthChunkError> {
    let token_result = tokenize(phonemes);
    if token_result.tokens.is_empty() {
        return Err(SynthChunkError::Skipped);
    }
    let token_len = token_result.tokens.len();
    if token_len > MAX_PHONEME_LENGTH {
        return Err(SynthChunkError::Failed(invalid_request(format!(
            "tokenised phonemes exceed Kokoro max length {MAX_PHONEME_LENGTH}",
        ))));
    }
    let input_ids = pad_with_zero(&token_result.tokens);
    let style_row = match voice.style_for_token_len(token_len) {
        Some(s) => s.to_vec(),
        None => {
            return Err(SynthChunkError::Failed(invalid_request(format!(
                "voice style table does not cover token-length {token_len} (max {})",
                crate::synth::voices::VOICE_STYLE_LENGTHS,
            ))));
        }
    };

    let t0 = Instant::now();
    let audio = match kokoro.synthesize(&input_ids, &style_row, speed) {
        Ok(a) => a,
        Err(KokoroInferError::Run(msg)) => {
            return Err(SynthChunkError::Failed(inference_failed(format!(
                "kokoro.run: {msg}"
            ))));
        }
        Err(KokoroInferError::OutputShape(msg)) => {
            return Err(SynthChunkError::Failed(inference_failed(format!(
                "kokoro output: {msg}"
            ))));
        }
    };
    let inference_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let audio_seconds = audio.len() as f32 / KOKORO_SAMPLE_RATE as f32;
    let audio_samples = audio.len();

    let wav = match encode_wav_kokoro(&audio) {
        Ok(b) => b,
        Err(msg) => {
            return Err(SynthChunkError::Failed(inference_failed(format!(
                "WAV encode: {msg}"
            ))));
        }
    };
    let audio_b64 = encode_base64(&wav);

    Ok(ChunkOutput {
        audio_b64,
        audio_seconds,
        audio_samples,
        inference_ms,
        token_count: token_len,
        truncated: token_result.truncated,
    })
}

/// Get or lazy-load the `voices.npz` bundle. No VRAM lease — voices
/// fit in ~14 MB of heap which is well under the broker's
/// significant-resident threshold.
async fn ensure_voices() -> Result<Arc<Voices>, wylde_shared::ipc::IpcError> {
    if let Some(v) = state::kokoro_voices() {
        return Ok(v);
    }
    let cfg = Config::get();
    let voices_path = resolve_voices_path(cfg).ok_or_else(|| {
        model_not_loaded(
            "voices.npz not found in Kokoro snapshot — run the voice.download_models action first",
        )
    })?;
    let loaded = Voices::load(&voices_path).map_err(|e| match e {
        VoicesLoadError::NotFound(p) => {
            model_not_loaded(format!("voices.npz not found at {}", p.display()))
        }
        VoicesLoadError::Io(m) => inference_failed(format!("voices.npz I/O: {m}")),
        VoicesLoadError::Format(m) => inference_failed(format!("voices.npz format: {m}")),
        VoicesLoadError::VoiceEntry { voice, detail } => {
            inference_failed(format!("voices.npz voice {voice}: {detail}"))
        }
    })?;
    let arc = Arc::new(loaded);
    state::set_kokoro_voices(arc.clone());
    Ok(arc)
}

/// Get or lazy-load the Kokoro ONNX session. Acquires a DRAM lease
/// against the broker keyed on the Kokoro repo id — same pattern as
/// the Whisper encoder lease.
async fn ensure_kokoro() -> Result<Arc<KokoroSynth>, wylde_shared::ipc::IpcError> {
    if let Some(k) = state::kokoro_synth() {
        return Ok(k);
    }
    let cfg = Config::get();
    let model_path = resolve_model_path(cfg).ok_or_else(|| {
        model_not_loaded(
            "Kokoro model.onnx not found in HF cache — run the voice.download_models action first",
        )
    })?;
    let bytes_hint = std::fs::metadata(&model_path).map(|m| m.len()).ok(); // wylde-check: discard-result-ok
    let lease = match lease::acquire("kokoro-82M-v1.0", bytes_hint).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(
                "wylde-voice: kokoro DRAM lease acquisition failed ({}: {}); loading anyway",
                e.code,
                e.message
            );
            None
        }
    };
    let loaded = KokoroSynth::load(&model_path).map_err(|e| match e {
        KokoroLoadError::NotFound(p) => {
            model_not_loaded(format!("Kokoro model.onnx not found at {}", p.display()))
        }
        KokoroLoadError::SessionBuild(m) => {
            inference_failed(format!("kokoro ort session build: {m}"))
        }
    })?;
    let arc = Arc::new(loaded);
    state::set_kokoro_synth(arc.clone(), lease);
    Ok(arc)
}

/// Resolve the path to Kokoro's `model.onnx` inside the HF cache.
fn resolve_model_path(_cfg: &Config) -> Option<PathBuf> {
    let snap = first_kokoro_snapshot()?;
    for rel in ["onnx/model.onnx", "model.onnx"] {
        let candidate = snap.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the path to Kokoro's `voices.npz` — produced by
/// `Voice/download_models.py::fetch_kokoro`.
fn resolve_voices_path(_cfg: &Config) -> Option<PathBuf> {
    let snap = first_kokoro_snapshot()?;
    let candidate = snap.join("voices.npz");
    if candidate.exists() { Some(candidate) } else { None }
}

/// First snapshot dir for the Kokoro repo in the HF cache.
fn first_kokoro_snapshot() -> Option<PathBuf> {
    let cache_dir_name = "models--onnx-community--Kokoro-82M-v1.0-ONNX";
    let hf_root = std::env::var_os("HUGGINGFACE_HUB_CACHE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").map(|p| PathBuf::from(p).join("hub")))
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(".cache").join("huggingface").join("hub"))
        })?;
    let snapshots = hf_root.join(cache_dir_name).join("snapshots");
    let entries = std::fs::read_dir(&snapshots).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthesize_rejects_missing_phonemes() {
        let r = handle_synthesize(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn resolve_phonemes_accepts_text_via_g2p() {
        // Slice 2: plain text is now phonemised in-process rather than
        // rejected. The resolver returns a non-empty IPA string that
        // tokenises to real Kokoro tokens.
        let ph = resolve_phonemes(&json!({"text": "Hello world"}), "af_heart")
            .expect("text should phonemise");
        assert!(!ph.trim().is_empty(), "G2P produced nothing: {ph:?}");
        assert!(!tokenize(&ph).tokens.is_empty(), "no tokens from {ph:?}");
    }

    #[test]
    fn resolve_phonemes_prefers_explicit_phonemes() {
        // When both are supplied, the explicit phoneme string wins (the
        // parity-test / direct-caller path) — text is not re-phonemised.
        let ph = resolve_phonemes(
            &json!({"text": "ignored", "phonemes": "həlˈoʊ"}),
            "af_heart",
        )
        .expect("explicit phonemes accepted");
        assert_eq!(ph, "həlˈoʊ");
    }

    #[test]
    fn resolve_phonemes_errors_when_neither_field_present() {
        let err = resolve_phonemes(&json!({"voice": "af_heart"}), "af_heart")
            .expect_err("no text/phonemes → error");
        assert_eq!(err.code, "invalid_request");
    }

    #[tokio::test]
    async fn synthesize_rejects_blank_phonemes() {
        let r = handle_synthesize(json!({"phonemes": "   "})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn synthesize_rejects_no_in_vocab_chars() {
        // Russian Cyrillic isn't in the Kokoro vocab — should land on
        // the "nothing to synthesize" branch.
        let r = handle_synthesize(json!({"phonemes": "ьюя"})).await;
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_request");
        assert!(err.message.contains("no in-vocab"), "{}", err.message);
    }

    #[tokio::test]
    async fn synthesize_stream_rejects_missing_phonemes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_synthesize_stream(json!({}), tx).await;
        let chunk = rx.recv().await.expect("at least one chunk");
        let err = chunk.expect_err("missing phonemes → invalid_request");
        assert_eq!(err.code, "invalid_request");
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn resolve_phonemes_british_voice_uses_gb_dialect() {
        // A `b…` voice routes through the en-GB lexicon. We don't assert
        // exact phonemes (dialect tables differ) — just that both dialects
        // produce in-vocab tokens, exercising the GB engine init path.
        let gb = resolve_phonemes(&json!({"text": "schedule"}), "bf_emma")
            .expect("GB text phonemises");
        assert!(!tokenize(&gb).tokens.is_empty(), "no GB tokens: {gb:?}");
    }

    #[tokio::test]
    async fn synthesize_stream_rejects_blank_phonemes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_synthesize_stream(json!({"phonemes": "   "}), tx).await;
        let chunk = rx.recv().await.expect("at least one chunk");
        assert_eq!(chunk.unwrap_err().code, "invalid_request");
    }

}
