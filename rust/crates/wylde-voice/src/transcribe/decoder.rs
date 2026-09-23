//! `ort` session wrapper around the Whisper decoder ONNX.
//!
//! The decoder runs on the **CPU EP** in Slice 11.A+. The KV-cached
//! `decoder_with_past_model.onnx` variant + NPU dynamic-shape decoder
//! support are both meaningful refinements but are out of scope here:
//! KV-cache plumbing requires shuttling `4 layers × 2 (k,v) × 2 (self,cross)
//! = 16` extra tensors per step, and the NPU dynamic-shape decoder is the
//! exact pathology the Phase 10 spike documented in
//! [`docs/wylde-voice-npu-spike-findings.md`].
//!
//! Strategy: the onnx-community `decoder_model.onnx` (no-past variant)
//! accepts the full `input_ids` sequence each step and returns logits
//! `[batch, seq, vocab]`. We argmax over the last position to pick the
//! next token. This is O(N²) total but with whisper-tiny.en (4 layers,
//! d_model=384) and a typical ≤30-token transcription, the wall-clock is
//! a few hundred milliseconds — comfortably under the 30 s timeout.
//!
//! The encoder hidden states (`[1, 1500, 384]` for tiny.en) are passed
//! as a static input each step. The decoder's own KV-cache outputs are
//! discarded — only the `logits` tensor is consumed.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use thiserror::Error;

use crate::transcribe::whisper::WhisperInferError;

#[derive(Debug, Error)]
pub enum DecoderLoadError {
    #[error("decoder ONNX not found at {0}")]
    NotFound(PathBuf),

    #[error("ort session build failed: {0}")]
    SessionBuild(String),
}

/// Decoder ONNX wrapper. Holds a CPU-EP `ort::Session` behind a mutex
/// (`Session::run` is `&mut`); one wrapper instance is reused across
/// every `voice.transcribe` call.
pub struct WhisperDecoder {
    session: Mutex<Session>,
    decoder_path: PathBuf,
    vocab_size: usize,
}

impl WhisperDecoder {
    /// Build a session from the decoder ONNX at `decoder_path`. Always
    /// uses the stock CPU EP — see module docs for why.
    pub fn load(decoder_path: &Path) -> Result<Self, DecoderLoadError> {
        if !decoder_path.exists() {
            return Err(DecoderLoadError::NotFound(decoder_path.to_path_buf()));
        }

        let session = Session::builder()
            .map_err(|e| DecoderLoadError::SessionBuild(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| DecoderLoadError::SessionBuild(e.to_string()))?
            .with_intra_threads(4)
            .map_err(|e| DecoderLoadError::SessionBuild(e.to_string()))?
            .commit_from_file(decoder_path)
            .map_err(|e| DecoderLoadError::SessionBuild(e.to_string()))?;

        // Vocab size is the last dimension of `logits`. We could inspect
        // the model's output metadata via `session.outputs()` but the
        // value is also pinned in the model's config.json
        // (`vocab_size: 51864` for whisper-tiny.en); we capture it via
        // the first inference call rather than parsing JSON here.
        Ok(Self {
            session: Mutex::new(session),
            decoder_path: decoder_path.to_path_buf(),
            vocab_size: 0,
        })
    }

    pub fn decoder_path(&self) -> &Path {
        &self.decoder_path
    }

    /// Run greedy autoregressive decoding.
    ///
    /// `prompt_tokens` is the seeded prefix (`<|startoftranscript|>` plus
    /// the language / task / no-timestamps trio for multilingual models,
    /// or `<|startoftranscript|><|notimestamps|>` for English-only). The
    /// generated tokens are appended; the function returns the prompt +
    /// generated IDs concatenated, so the caller can slice off the prompt
    /// before detokenisation. Generation halts at `eot_token` or after
    /// `max_new_tokens` total new tokens, whichever comes first.
    ///
    /// `encoder_hidden_states` is the encoder output tensor with shape
    /// `[1, encoder_seq, d_model]` — e.g. `[1, 1500, 384]` for
    /// whisper-tiny.en. The buffer is consumed as a row-major
    /// `[batch, seq, d_model]` view.
    pub fn generate(
        &self,
        prompt_tokens: &[i64],
        encoder_hidden_states: &[f32],
        encoder_shape: &[i64],
        eot_token: i64,
        max_new_tokens: usize,
    ) -> Result<Vec<i64>, WhisperInferError> {
        self.generate_with_callback(
            prompt_tokens,
            encoder_hidden_states,
            encoder_shape,
            eot_token,
            max_new_tokens,
            |_| ControlFlow::Continue(()),
        )
    }

    /// Streaming-friendly variant of [`Self::generate`]. After each newly
    /// decoded token is appended to the running sequence, `on_token` is
    /// invoked with that token id. Return [`ControlFlow::Break`] to halt
    /// early (the function returns the partial sequence including the
    /// callback-observed token); return [`ControlFlow::Continue`] to keep
    /// decoding. EOT and `max_new_tokens` stop conditions remain in force
    /// independently — the callback only adds cooperative early-exit
    /// (e.g. for client cancellation in `voice.transcribe_stream`).
    pub fn generate_with_callback<F>(
        &self,
        prompt_tokens: &[i64],
        encoder_hidden_states: &[f32],
        encoder_shape: &[i64],
        eot_token: i64,
        max_new_tokens: usize,
        mut on_token: F,
    ) -> Result<Vec<i64>, WhisperInferError>
    where
        F: FnMut(i64) -> ControlFlow<()>,
    {
        if encoder_shape.len() != 3 || encoder_shape[0] != 1 {
            return Err(WhisperInferError::Run(format!(
                "encoder hidden states must be [1, seq, d_model], got {encoder_shape:?}"
            )));
        }
        let expected_len = encoder_shape.iter().product::<i64>() as usize;
        if encoder_hidden_states.len() != expected_len {
            return Err(WhisperInferError::Run(format!(
                "encoder hidden states buffer size {} != prod(shape {expected_len})",
                encoder_hidden_states.len()
            )));
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| WhisperInferError::Run("decoder mutex poisoned".to_owned()))?;

        let mut tokens: Vec<i64> = prompt_tokens.to_vec();

        for _ in 0..max_new_tokens {
            let input_shape: Vec<i64> = vec![1, tokens.len() as i64];
            let input_ids = TensorRef::from_array_view((input_shape, tokens.as_slice()))
                .map_err(|e| WhisperInferError::Run(format!("input_ids tensor: {e}")))?;
            let enc_input =
                TensorRef::from_array_view((encoder_shape.to_vec(), encoder_hidden_states))
                    .map_err(|e| {
                        WhisperInferError::Run(format!("encoder_hidden_states tensor: {e}"))
                    })?;

            let outputs = session
                .run(inputs![
                    "input_ids" => input_ids,
                    "encoder_hidden_states" => enc_input,
                ])
                .map_err(|e| WhisperInferError::Run(e.to_string()))?;

            let logits = outputs.get("logits").ok_or_else(|| {
                WhisperInferError::OutputShape("decoder missing logits".to_owned())
            })?;
            let (logits_shape, logits_data) = logits
                .try_extract_tensor::<f32>()
                .map_err(|e| WhisperInferError::OutputShape(e.to_string()))?;

            // Logits shape: [batch=1, seq, vocab]. We argmax over the
            // last-position row.
            if logits_shape.len() != 3 || logits_shape[0] != 1 {
                return Err(WhisperInferError::OutputShape(format!(
                    "unexpected logits shape: {logits_shape:?}"
                )));
            }
            let seq = logits_shape[1] as usize;
            let vocab = logits_shape[2] as usize;
            if seq == 0 || logits_data.len() < seq * vocab {
                return Err(WhisperInferError::OutputShape(format!(
                    "logits data size {} too small for [{seq}, {vocab}]",
                    logits_data.len()
                )));
            }

            let last_row = &logits_data[(seq - 1) * vocab..seq * vocab];
            let next = argmax(last_row);
            tokens.push(next);
            if next == eot_token {
                break;
            }
            if on_token(next).is_break() {
                break;
            }
        }

        Ok(tokens)
    }

    /// Report the decoder ONNX's vocab size as reported by the most
    /// recent inference call (0 if no call has happened yet).
    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }
}

/// Argmax over an f32 slice. Returns the index of the largest element;
/// ties go to the lowest index (which matches NumPy / PyTorch convention).
fn argmax(row: &[f32]) -> i64 {
    let mut best_idx = 0_usize;
    let mut best_val = f32::MIN;
    for (i, &v) in row.iter().enumerate() {
        if v > best_val {
            best_val = v;
            best_idx = i;
        }
    }
    best_idx as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_returns_first_max_index() {
        assert_eq!(argmax(&[0.0, 1.0, 0.5]), 1);
        assert_eq!(argmax(&[2.0, 1.0, 2.0]), 0);
        assert_eq!(argmax(&[-1.0, -2.0, -0.5]), 2);
    }

    #[test]
    fn load_missing_file_returns_not_found() {
        let result = WhisperDecoder::load(Path::new("/no/such/decoder.onnx"));
        assert!(matches!(result, Err(DecoderLoadError::NotFound(_))));
    }
}
