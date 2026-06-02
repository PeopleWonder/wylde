//! `voice.transcribe` — WAV bytes → text.
//!
//! ## Slice 11.A+ scope
//!
//! End-to-end: decode the supplied WAV, compute the `[1, 80, 3000]` log-mel,
//! run the Whisper encoder (CPU EP by default; OpenVINO NPU EP when
//! `WYLDE_VOICE_WHISPER_BACKEND=npu`), then drive the Whisper decoder
//! (CPU EP) in a greedy-sampling loop with the tokenizer-built prompt,
//! detokenise to text. The reply now carries the actual transcript plus
//! latency / token-count breakdown.
//!
//! ## What's deliberately still deferred
//!
//! * KV-cached decoder (`decoder_with_past_model.onnx`) — would cut
//!   decoder wall-clock 2-4× on longer transcripts; current no-past
//!   decoder is O(N²) per call but plenty fast for ≤30 s utterances.
//! * NPU decoder — the dynamic-shape decoder graph is the exact case
//!   the OpenVINO VPUX compiler refuses; CPU EP is the right call.
//! * Beam search — greedy (`beam_size=1`) matches the Python pipeline's
//!   latency-mode default; beam search is a later refinement.
//! * Streaming token output (`voice.transcribe_stream`) — Slice 11.C
//!   shipped: one full WAV in, partial transcripts streamed out as the
//!   decoder produces tokens. Audio-chunked input is still deferred to
//!   the cpal mic capture work in Slice 11.D, since the IPC streaming
//!   primitive is one-payload-in / many-chunks-out by design.
//! * Mic capture — Slice 11.D wires `cpal`.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::actions::error::{
    audio_decode_failed, inference_failed, invalid_request, model_not_loaded, npu_unavailable,
};
use crate::config::Config;
#[cfg(test)]
use crate::config::SttBackend;
use crate::lease;
use crate::state;
use crate::transcribe::audio::decode_wav;
use crate::transcribe::decoder::{DecoderLoadError, WhisperDecoder};
use crate::transcribe::mel::compute_log_mel;
use crate::transcribe::tokenizer::{TokenizerLoadError, WhisperTokenizer};
use crate::transcribe::whisper::{WhisperEncoder, WhisperInferError, WhisperLoadError};

/// Default cap on generated tokens — matches Whisper's `max_length=448`
/// from `generation_config.json`. Callers can override via
/// `payload.max_new_tokens` when they want a tighter latency budget.
const DEFAULT_MAX_NEW_TOKENS: usize = 448;

/// Default language label surfaced in the reply for English-only models.
/// The actual language-detection path (where the first decoder token is
/// the language classifier) is wired through `build_prompt(lang)`; for
/// `*.en` models the tokenizer just ignores the lang and emits the
/// 2-token English prompt.
const DEFAULT_LANGUAGE: &str = "en";

/// `voice.transcribe` payload schema:
/// ```jsonc
/// {
///   "audio_path":     "C:/path/to/utterance.wav",   // OR
///   "audio_b64":      "<base64-encoded WAV bytes>",
///   "language":       "en",                          // optional, defaults to "en"
///   "max_new_tokens": 448                            // optional
/// }
/// ```
pub async fn handle_transcribe(payload: Value) -> Reply {
    let bytes = match read_audio_payload(&payload) {
        Ok(b) => b,
        Err(e) => return Reply::err(e),
    };

    let pcm = match decode_wav(&bytes) {
        Ok(p) => p,
        Err(e) => return Reply::err(audio_decode_failed(format!("WAV decode: {e}"))),
    };
    let duration_s = pcm.len() as f32 / crate::transcribe::audio::WHISPER_SAMPLE_RATE as f32;

    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_LANGUAGE)
        .to_owned();
    let max_new_tokens = payload
        .get("max_new_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_NEW_TOKENS);

    let encoder = match ensure_encoder().await {
        Ok(enc) => enc,
        Err(e) => return Reply::err(e),
    };
    let decoder = match ensure_decoder().await {
        Ok(dec) => dec,
        Err(e) => return Reply::err(e),
    };
    let tokenizer = match ensure_tokenizer().await {
        Ok(tok) => tok,
        Err(e) => return Reply::err(e),
    };

    let mel = compute_log_mel(&pcm);

    let t_enc = Instant::now();
    let enc_out = match encoder.run_encoder(&mel) {
        Ok(o) => o,
        Err(WhisperInferError::Run(msg)) => {
            return Reply::err(inference_failed(format!("encoder.run: {msg}")));
        }
        Err(WhisperInferError::OutputShape(msg)) => {
            return Reply::err(inference_failed(format!("encoder output: {msg}")));
        }
    };
    let encoder_inference_ms = t_enc.elapsed().as_secs_f64() * 1000.0;

    let prompt = match tokenizer.build_prompt(&language) {
        Ok(p) => p,
        Err(e) => return Reply::err(invalid_request(format!("build_prompt({language}): {e}"))),
    };
    let prompt_len = prompt.len();

    let t_dec = Instant::now();
    let token_ids = match decoder.generate(
        &prompt,
        &enc_out.hidden_states,
        &enc_out.shape,
        tokenizer.eot_id(),
        max_new_tokens,
    ) {
        Ok(ids) => ids,
        Err(WhisperInferError::Run(msg)) => {
            return Reply::err(inference_failed(format!("decoder.generate: {msg}")));
        }
        Err(WhisperInferError::OutputShape(msg)) => {
            return Reply::err(inference_failed(format!("decoder output: {msg}")));
        }
    };
    let decoder_inference_ms = t_dec.elapsed().as_secs_f64() * 1000.0;
    let total_inference_ms = encoder_inference_ms + decoder_inference_ms;
    let generated = &token_ids[prompt_len..];

    let text = match tokenizer.decode(generated) {
        Ok(t) => t,
        Err(e) => return Reply::err(inference_failed(format!("tokenizer.decode: {e}"))),
    };

    Reply::ok(json!({
        "audio_seconds": duration_s,
        "device": encoder.device(),
        "backend": encoder.backend().as_str(),
        "encoder_output_shape": enc_out.shape,
        "encoder_inference_ms": encoder_inference_ms,
        "decoder_inference_ms": decoder_inference_ms,
        "total_inference_ms": total_inference_ms,
        "text": text.trim(),
        "language": language,
        "token_count": generated.len(),
    }))
}

/// `voice.transcribe_stream` — streaming variant. Same payload schema as
/// [`handle_transcribe`]. Emits one chunk per stage of the pipeline so
/// callers can render partial transcripts live:
///
/// ```jsonc
/// // After the encoder runs (once per stream):
/// {"type": "encoder_complete", "audio_seconds": 5.6, "encoder_inference_ms": 23.4,
///  "encoder_output_shape": [1, 1500, 384], "device": "CPU", "backend": "cpu",
///  "language": "en", "max_new_tokens": 448}
/// // While the decoder argmax-loops (one chunk per emitted token):
/// {"type": "token", "index": 0, "token_id": 50362, "delta": " Hello"}
/// {"type": "token", "index": 1, "token_id": 11, "delta": ","}
/// // ...
/// // Final summary after EOT or max_new_tokens:
/// {"type": "transcript_complete", "text": "Hello, world.", "token_count": 4,
///  "decoder_inference_ms": 87.2, "total_inference_ms": 110.6,
///  "audio_seconds": 5.6, "language": "en", "device": "CPU", "backend": "cpu"}
/// ```
///
/// The IPC layer adds the `done=true` terminator after the handler
/// returns — callers should not assume the `transcript_complete` chunk
/// is the last byte they'll see, only the last *handler-emitted* one.
pub async fn handle_transcribe_stream(payload: Value, sender: StreamSender) {
    let bytes = match read_audio_payload(&payload) {
        Ok(b) => b,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    let pcm = match crate::transcribe::audio::decode_wav(&bytes) {
        Ok(p) => p,
        Err(e) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(audio_decode_failed(format!("WAV decode: {e}"))))
                .await;
            return;
        }
    };
    let duration_s = pcm.len() as f32 / crate::transcribe::audio::WHISPER_SAMPLE_RATE as f32;

    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_LANGUAGE)
        .to_owned();
    let max_new_tokens = payload
        .get("max_new_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_NEW_TOKENS);

    let encoder = match ensure_encoder().await {
        Ok(enc) => enc,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };
    let decoder = match ensure_decoder().await {
        Ok(dec) => dec,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };
    let tokenizer = match ensure_tokenizer().await {
        Ok(tok) => tok,
        Err(e) => {
            let _ = sender.send(Err(e)).await; // wylde-check: discard-result-ok
            return;
        }
    };

    let mel = crate::transcribe::mel::compute_log_mel(&pcm);
    let t_enc = Instant::now();
    let enc_out = match encoder.run_encoder(&mel) {
        Ok(o) => o,
        Err(crate::transcribe::whisper::WhisperInferError::Run(msg)) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(inference_failed(format!("encoder.run: {msg}"))))
                .await;
            return;
        }
        Err(crate::transcribe::whisper::WhisperInferError::OutputShape(msg)) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(inference_failed(format!("encoder output: {msg}"))))
                .await;
            return;
        }
    };
    let encoder_inference_ms = t_enc.elapsed().as_secs_f64() * 1000.0;

    if sender
        .send(Ok(json!({
            "type": "encoder_complete",
            "audio_seconds": duration_s,
            "encoder_inference_ms": encoder_inference_ms,
            "encoder_output_shape": enc_out.shape,
            "device": encoder.device(),
            "backend": encoder.backend().as_str(),
            "language": language,
            "max_new_tokens": max_new_tokens,
        })))
        .await
        .is_err()
    {
        return;
    }

    let prompt = match tokenizer.build_prompt(&language) {
        Ok(p) => p,
        Err(e) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(invalid_request(format!("build_prompt({language}): {e}"))))
                .await;
            return;
        }
    };
    let prompt_len = prompt.len();

    // The decoder loop is synchronous and CPU-bound (it sits inside an
    // `ort::Session::run` call per token). Hand it off to a blocking
    // thread so this async task — and therefore the StreamSender — stays
    // schedulable for `closed()` polling + heartbeat emission.
    let decoder_arc = Arc::clone(&decoder);
    let prompt_for_loop = prompt.clone();
    let enc_hidden = enc_out.hidden_states.clone();
    let enc_shape = enc_out.shape.clone();
    let eot = tokenizer.eot_id();
    let tokenizer_for_decode = Arc::clone(&tokenizer);

    let (tok_tx, mut tok_rx) = tokio::sync::mpsc::channel::<i64>(64);
    let sender_for_closed = sender.clone();
    let t_dec = Instant::now();

    let join = tokio::task::spawn_blocking(move || {
        decoder_arc.generate_with_callback(
            &prompt_for_loop,
            &enc_hidden,
            &enc_shape,
            eot,
            max_new_tokens,
            |tok| {
                // Best-effort delivery to the async side. If the bounded
                // channel is full, drop and break — the stream is going
                // to die at the client anyway.
                match tok_tx.blocking_send(tok) {
                    Ok(()) => {
                        if sender_for_closed.is_closed() {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    }
                    Err(_) => ControlFlow::Break(()),
                }
            },
        )
    });

    let mut all_tokens: Vec<i64> = prompt;
    let mut last_decoded_len = 0_usize;
    let mut emitted = 0_usize;
    let mut cancelled = false;
    while let Some(tok) = tok_rx.recv().await {
        all_tokens.push(tok);
        let generated_so_far = &all_tokens[prompt_len..];
        // Decode the FULL generated sequence so far, then emit just the
        // delta vs the previous decode. Whisper BPE has multi-piece
        // tokens whose per-token decode is meaningless in isolation, so
        // cumulative decode + slice is the canonical streaming pattern.
        let text_so_far = match tokenizer_for_decode.decode(generated_so_far) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let delta = if text_so_far.len() > last_decoded_len {
            text_so_far[last_decoded_len..].to_owned()
        } else {
            String::new()
        };
        last_decoded_len = text_so_far.len();
        let send_result = sender
            .send(Ok(json!({
                "type": "token",
                "index": emitted,
                "token_id": tok,
                "delta": delta,
            })))
            .await;
        emitted += 1;
        if send_result.is_err() {
            cancelled = true;
            break;
        }
    }

    let token_ids = match join.await {
        Ok(Ok(ids)) => ids,
        Ok(Err(crate::transcribe::whisper::WhisperInferError::Run(msg))) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(inference_failed(format!("decoder.generate: {msg}"))))
                .await;
            return;
        }
        Ok(Err(crate::transcribe::whisper::WhisperInferError::OutputShape(msg))) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(inference_failed(format!("decoder output: {msg}"))))
                .await;
            return;
        }
        Err(join_err) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(inference_failed(format!(
                    "decoder spawn_blocking joined with error: {join_err}"
                ))))
                .await;
            return;
        }
    };
    let decoder_inference_ms = t_dec.elapsed().as_secs_f64() * 1000.0;
    let total_inference_ms = encoder_inference_ms + decoder_inference_ms;
    let generated = &token_ids[prompt_len..];

    let text = match tokenizer.decode(generated) {
        Ok(t) => t,
        Err(e) => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(inference_failed(format!("tokenizer.decode: {e}"))))
                .await;
            return;
        }
    };

    if cancelled {
        return;
    }

    let _ = sender // wylde-check: discard-result-ok
        .send(Ok(json!({
            "type": "transcript_complete",
            "text": text.trim(),
            "token_count": generated.len(),
            "decoder_inference_ms": decoder_inference_ms,
            "total_inference_ms": total_inference_ms,
            "audio_seconds": duration_s,
            "language": language,
            "device": encoder.device(),
            "backend": encoder.backend().as_str(),
        })))
        .await;
}

fn read_audio_payload(payload: &Value) -> Result<Vec<u8>, IpcError> {
    if let Some(path) = payload.get("audio_path").and_then(Value::as_str) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(invalid_request("payload.audio_path is empty"));
        }
        return std::fs::read(trimmed)
            .map_err(|e| audio_decode_failed(format!("read audio_path {trimmed}: {e}")));
    }

    if let Some(b64) = payload.get("audio_b64").and_then(Value::as_str) {
        let trimmed = b64.trim();
        if trimmed.is_empty() {
            return Err(invalid_request("payload.audio_b64 is empty"));
        }
        return decode_base64(trimmed)
            .map_err(|e| invalid_request(format!("audio_b64: {e}")));
    }

    Err(invalid_request(
        "payload must include either audio_path or audio_b64",
    ))
}

/// Get or lazy-load the Whisper encoder. Holds a VRAM lease while
/// loaded — released when the service stops or the encoder is unloaded.
async fn ensure_encoder() -> Result<Arc<WhisperEncoder>, wylde_shared::ipc::IpcError> {
    if let Some(enc) = state::whisper_encoder() {
        return Ok(enc);
    }

    let cfg = Config::get();
    let encoder_path = resolve_encoder_path(cfg)
        .ok_or_else(|| {
            model_not_loaded(format!(
                "no encoder ONNX found for {} (override via WYLDE_VOICE_STT_ENCODER_PATH; \
                 first-run setup downloads + ONNX-exports the configured whisper model)",
                cfg.stt_model
            ))
        })?;

    let bytes_hint = std::fs::metadata(&encoder_path).map(|m| m.len()).ok(); // wylde-check: discard-result-ok
    let lease = match lease::acquire(&format!("{}#encoder", cfg.stt_model), bytes_hint).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(
                "wylde-voice: encoder VRAM lease acquisition failed ({}: {}); loading anyway",
                e.code,
                e.message
            );
            None
        }
    };

    let loaded = WhisperEncoder::load(
        &encoder_path,
        cfg.stt_backend,
        &cfg.ov_device,
        &cfg.ov_cache_dir,
    )
    .map_err(|e| match e {
        WhisperLoadError::NotFound(p) => {
            model_not_loaded(format!("encoder ONNX not found at {}", p.display()))
        }
        WhisperLoadError::OpenVinoUnavailable(m) => {
            npu_unavailable(m)
        }
        WhisperLoadError::SessionBuild(m) => {
            inference_failed(format!("ort session build: {m}"))
        }
    })?;

    let arc = Arc::new(loaded);
    state::set_whisper_encoder(arc.clone(), lease);
    Ok(arc)
}

/// Get or lazy-load the Whisper decoder. Held in DRAM via the broker's
/// `vram.reserve` (the broker is shape-agnostic about NPU vs DRAM — it
/// accounts for resident model bytes, so the decoder lease shows up
/// alongside the encoder one in `vram.list`). CPU-only — see
/// [`crate::transcribe::decoder`] for why.
async fn ensure_decoder() -> Result<Arc<WhisperDecoder>, wylde_shared::ipc::IpcError> {
    if let Some(dec) = state::whisper_decoder() {
        return Ok(dec);
    }

    let cfg = Config::get();
    let decoder_path = resolve_decoder_path(cfg)
        .ok_or_else(|| {
            model_not_loaded(format!(
                "no decoder ONNX found for {} (expected `decoder_model.onnx` next to \
                 the encoder; first-run setup downloads + ONNX-exports the model)",
                cfg.stt_model
            ))
        })?;

    let bytes_hint = std::fs::metadata(&decoder_path).map(|m| m.len()).ok(); // wylde-check: discard-result-ok
    let lease = match lease::acquire(&format!("{}#decoder", cfg.stt_model), bytes_hint).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(
                "wylde-voice: decoder DRAM lease acquisition failed ({}: {}); loading anyway",
                e.code,
                e.message
            );
            None
        }
    };

    let loaded = WhisperDecoder::load(&decoder_path).map_err(|e| match e {
        DecoderLoadError::NotFound(p) => {
            model_not_loaded(format!("decoder ONNX not found at {}", p.display()))
        }
        DecoderLoadError::SessionBuild(m) => {
            inference_failed(format!("decoder ort session build: {m}"))
        }
    })?;

    let arc = Arc::new(loaded);
    state::set_whisper_decoder(arc.clone(), lease);
    Ok(arc)
}

/// Get or lazy-load the Whisper tokenizer. No VRAM lease — the
/// tokenizer.json is < 5 MB and lives entirely in Rust heap.
async fn ensure_tokenizer() -> Result<Arc<WhisperTokenizer>, wylde_shared::ipc::IpcError> {
    if let Some(tok) = state::whisper_tokenizer() {
        return Ok(tok);
    }

    let cfg = Config::get();
    let tokenizer_path = resolve_tokenizer_path(cfg).ok_or_else(|| {
        model_not_loaded(format!(
            "no tokenizer.json found for {} in the HF cache (expected next to \
             config.json in the snapshot dir)",
            cfg.stt_model
        ))
    })?;
    let is_multilingual = read_is_multilingual(cfg).unwrap_or(false);

    let loaded = WhisperTokenizer::load(&tokenizer_path, is_multilingual).map_err(|e| match e {
        TokenizerLoadError::NotFound(p) => {
            model_not_loaded(format!("tokenizer.json not found at {}", p.display()))
        }
        TokenizerLoadError::Load(m) => {
            inference_failed(format!("tokenizer load: {m}"))
        }
        TokenizerLoadError::MissingSpecialToken(t) => {
            inference_failed(format!("tokenizer vocab missing required special token: {t}"))
        }
    })?;

    let arc = Arc::new(loaded);
    state::set_whisper_tokenizer(arc.clone());
    Ok(arc)
}

/// Resolve the encoder ONNX path: explicit override first, then a
/// conventional HF-cache-relative location.
fn resolve_encoder_path(cfg: &Config) -> Option<PathBuf> {
    if let Some(p) = &cfg.stt_encoder_path_override {
        if p.exists() {
            return Some(p.clone());
        }
    }
    let snap = first_model_snapshot(cfg)?;
    for rel in ["encoder_model.onnx", "onnx/encoder_model.onnx"] {
        let candidate = snap.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the decoder ONNX path. Lives alongside the encoder in the
/// same snapshot directory (onnx-community export convention).
fn resolve_decoder_path(cfg: &Config) -> Option<PathBuf> {
    let snap = first_model_snapshot(cfg)?;
    for rel in ["decoder_model.onnx", "onnx/decoder_model.onnx"] {
        let candidate = snap.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the tokenizer.json path. Sits next to `config.json` at the
/// snapshot root.
fn resolve_tokenizer_path(cfg: &Config) -> Option<PathBuf> {
    let snap = first_model_snapshot(cfg)?;
    let p = snap.join("tokenizer.json");
    if p.exists() { Some(p) } else { None }
}

/// Whisper's `*.en` config.json forces a 1-element `forced_decoder_ids`
/// (`[[1, 50362]]` — notimestamps directly at position 1). Multilingual
/// variants force a longer sequence (typically 3 elements including a
/// language and `<|transcribe|>` token). The tokenizer ships the same
/// vocab for both, so this is the canonical signal for "do I need the
/// 4-token prompt vs the 2-token prompt".
///
/// Returns `Some(true)` for multilingual, `Some(false)` for English-only,
/// `None` when the config can't be read (caller treats as English-only).
fn read_is_multilingual(cfg: &Config) -> Option<bool> {
    let snap = first_model_snapshot(cfg)?;
    let cfg_path = snap.join("config.json");
    let raw = std::fs::read_to_string(&cfg_path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let forced = v.get("forced_decoder_ids")?.as_array()?;
    // 1 element → English-only; >1 → multilingual.
    Some(forced.len() > 1)
}

/// First snapshot directory in the HF cache for the configured model.
/// Returns `None` when the model hasn't been downloaded yet.
fn first_model_snapshot(cfg: &Config) -> Option<PathBuf> {
    let cache_dir_name = format!("models--{}", cfg.stt_model.replace('/', "--"));
    let hf_root = std::env::var_os("HUGGINGFACE_HUB_CACHE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").map(|p| PathBuf::from(p).join("hub")))
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(".cache").join("huggingface").join("hub"))
        })?;
    let snapshots = hf_root.join(&cache_dir_name).join("snapshots");
    let entries = std::fs::read_dir(&snapshots).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// Minimal base64 decode (URL-safe and standard alphabets, with or
/// without padding). Saves pulling in a dep for what is conceptually
/// trivial — this is the same shape Python's `base64.b64decode(s,
/// validate=False)` accepts on the Voice/run.py side.
fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() {
        return Err("empty after stripping whitespace".to_owned());
    }
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    let bytes = cleaned.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let mut chunk = [0u8; 4];
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
                b'+' | b'-' => 62,
                b'/' | b'_' => 63,
                b'=' => {
                    pad += 1;
                    b'='
                }
                _ => return Err(format!("invalid base64 character: {c:?}")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_cfg(stt_model: &str, override_enc: Option<PathBuf>) -> Config {
        Config {
            wylde_root: PathBuf::from("."),
            stt_backend: SttBackend::Cpu,
            stt_model: stt_model.to_owned(),
            stt_encoder_path_override: override_enc,
            ov_device: "CPU".to_owned(),
            ov_cache_dir: PathBuf::from("./ov_cache"),
            tts_voice: "af_heart".to_owned(),
            tts_speed: 1.0,
            broker_service: "wylde-vram-broker".to_owned(),
            default_priority: 50,
            lease_ttl_s: 60.0,
            lease_heartbeat_s: 20,
            health_timeout_s: 3,
            transcribe_timeout_s: 30,
            wakeword_model: "openWakeWord/hey-jarvis".to_owned(),
            wakeword_models_dir: PathBuf::from("./wakeword"),
            vad_threshold: crate::vad::DEFAULT_THRESHOLD,
            vad_silence_timeout_ms: crate::vad::DEFAULT_SILENCE_TIMEOUT_MS,
        }
    }

    #[tokio::test]
    async fn transcribe_rejects_missing_audio_fields() {
        let r = handle_transcribe(json!({})).await;
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_request");
        assert!(
            err.message.contains("audio_path") && err.message.contains("audio_b64"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn transcribe_rejects_blank_audio_path() {
        let r = handle_transcribe(json!({"audio_path": "  "})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn transcribe_surfaces_audio_decode_failed_on_missing_file() {
        let r = handle_transcribe(json!({
            "audio_path": "/no/such/file/wylde-voice-test.wav"
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "audio_decode_failed");
    }

    #[tokio::test]
    async fn transcribe_stream_emits_invalid_request_on_missing_audio() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_transcribe_stream(json!({}), tx).await;
        let chunk = rx.recv().await.expect("at least one error chunk");
        let err = chunk.expect_err("missing audio must surface as a stream-level error");
        assert_eq!(err.code, "invalid_request");
        assert!(rx.recv().await.is_none(), "no further chunks after error");
    }

    #[tokio::test]
    async fn transcribe_stream_surfaces_audio_decode_failed_on_missing_file() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_transcribe_stream(
            json!({"audio_path": "/no/such/file/wylde-voice-stream-test.wav"}),
            tx,
        )
        .await;
        let chunk = rx.recv().await.expect("at least one chunk");
        let err = chunk.expect_err("missing file must surface as audio_decode_failed");
        assert_eq!(err.code, "audio_decode_failed");
    }

    #[tokio::test]
    async fn transcribe_stream_rejects_blank_audio_path() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_transcribe_stream(json!({"audio_path": "   "}), tx).await;
        let chunk = rx.recv().await.expect("at least one chunk");
        let err = chunk.expect_err("blank path → invalid_request");
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn base64_decodes_known_value() {
        let out = decode_base64("SGVsbG8=").unwrap();
        assert_eq!(out, b"Hello");
        let out = decode_base64("SGVsbG8").unwrap();
        assert_eq!(out, b"Hello");
        let out = decode_base64("SGVs bG8=\n").unwrap();
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn base64_rejects_invalid_characters() {
        assert!(decode_base64("not!valid").is_err());
        assert!(decode_base64("").is_err());
        assert!(decode_base64("   ").is_err());
    }

    #[test]
    fn resolve_encoder_path_honours_override() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cfg = mk_cfg("openai/whisper-tiny.en", Some(tmp.path().to_path_buf()));
        let resolved = resolve_encoder_path(&cfg).unwrap();
        assert_eq!(resolved, tmp.path());
    }

    #[test]
    fn resolve_encoder_path_returns_none_for_missing_override_and_missing_repo() {
        let cfg = mk_cfg(
            "never-going-to-exist/fake-whisper",
            Some(PathBuf::from("/no/such/wylde-test-encoder.onnx")),
        );
        assert!(resolve_encoder_path(&cfg).is_none());
    }

    #[test]
    fn resolve_decoder_path_returns_none_for_missing_repo() {
        let cfg = mk_cfg("never-going-to-exist/fake-whisper", None);
        assert!(resolve_decoder_path(&cfg).is_none());
    }

    #[test]
    fn resolve_tokenizer_path_returns_none_for_missing_repo() {
        let cfg = mk_cfg("never-going-to-exist/fake-whisper", None);
        assert!(resolve_tokenizer_path(&cfg).is_none());
    }
}
