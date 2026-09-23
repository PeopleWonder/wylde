//! `ort` session wrapper around the Kokoro TTS ONNX.
//!
//! Direct counterpart of [`crate::transcribe::whisper::WhisperEncoder`] —
//! a CPU-only session with a per-call lock so the action handler can be
//! called concurrently without races on the `ort::Session`'s mutable
//! state. The Kokoro graph is sparse (~120 MB on disk) and runs ~real-
//! time on CPU at whisper-tiny scale, so we don't gate it behind the
//! OpenVINO feature: every `cargo build` of `wylde-voice` carries the
//! TTS code path live.
//!
//! ## Why CPU-only
//!
//! Phase 10's spike found that Kokoro's ONNX export — like Whisper's
//! decoder — has dynamic shape inputs (`input_ids` is `[1, N]` where N
//! varies per utterance). The OpenVINO VPUX compiler refuses dynamic
//! shapes, and HETERO:NPU,CPU partitioning offers nothing here because
//! the bulk of the work is concentrated in dynamic-shape ops anyway.
//! CPU is the right call for the foreseeable future; if a later
//! revision ships a static-shape Kokoro export, the same OpenVINO EP
//! attach pattern we use for the Whisper encoder will transplant
//! cleanly. Until then this session is unconditionally CPU.
//!
//! ## ONNX I/O contract
//!
//! Inputs (per `onnx-community/Kokoro-82M-v1.0-ONNX`):
//!   * `input_ids`  — int64, shape `[1, N+2]` (0-padded phonemes).
//!   * `style`      — float32, shape `[1, 256]` (per-length slice of the
//!     voice style table).
//!   * `speed`      — float32, shape `[1]`. Python's stock
//!     `kokoro_onnx.create` ships this as int32, which the export rejects
//!     on later runtimes; `Voice/synthesize.py::_patch_kokoro_speed`
//!     monkey-patches the dtype to float32. We just emit float32
//!     directly — no patch needed in Rust.
//!
//! Output: float32 mono PCM, shape `[T]`. Sample rate is 24 kHz
//! (`KOKORO_SAMPLE_RATE`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use thiserror::Error;

use crate::synth::voices::VOICE_STYLE_DIM;

#[derive(Debug, Error)]
pub enum KokoroLoadError {
    #[error("Kokoro ONNX not found at {0}")]
    NotFound(PathBuf),

    #[error("ort session build failed: {0}")]
    SessionBuild(String),
}

#[derive(Debug, Error)]
pub enum KokoroInferError {
    #[error("Kokoro run failed: {0}")]
    Run(String),

    #[error("Kokoro output missing or wrong dtype: {0}")]
    OutputShape(String),
}

/// A loaded Kokoro ONNX session. Same single-mutex shape as
/// [`crate::transcribe::whisper::WhisperEncoder`]: `ort::Session::run`
/// is `&mut`, and we serialise concurrent callers behind a
/// `parking_lot`-style guard via `std::sync::Mutex`.
pub struct KokoroSynth {
    session: Mutex<Session>,
    model_path: PathBuf,
}

impl KokoroSynth {
    /// Build the session from `model.onnx`. CPU EP only — see module
    /// docstring for why NPU is deferred.
    pub fn load(model_path: &Path) -> Result<Self, KokoroLoadError> {
        if !model_path.exists() {
            return Err(KokoroLoadError::NotFound(model_path.to_path_buf()));
        }
        let session = Session::builder()
            .map_err(|e| KokoroLoadError::SessionBuild(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| KokoroLoadError::SessionBuild(e.to_string()))?
            .with_intra_threads(4)
            .map_err(|e| KokoroLoadError::SessionBuild(e.to_string()))?
            .commit_from_file(model_path)
            .map_err(|e| KokoroLoadError::SessionBuild(e.to_string()))?;
        Ok(Self {
            session: Mutex::new(session),
            model_path: model_path.to_path_buf(),
        })
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Run one Kokoro inference pass. `input_ids` is the 0-padded
    /// token sequence (length N+2). `style` is the voice's `[1, 256]`
    /// row sliced at the un-padded token length. `speed` is the
    /// playback rate multiplier; Python clamps to [0.5, 2.0] —
    /// callers should do the same before reaching here.
    ///
    /// Returns the raw float32 mono PCM at 24 kHz.
    pub fn synthesize(
        &self,
        input_ids: &[i64],
        style: &[f32],
        speed: f32,
    ) -> Result<Vec<f32>, KokoroInferError> {
        if style.len() != VOICE_STYLE_DIM {
            return Err(KokoroInferError::Run(format!(
                "style row has wrong length: got {}, want {}",
                style.len(),
                VOICE_STYLE_DIM
            )));
        }
        let mut session = self
            .session
            .lock()
            .map_err(|_| KokoroInferError::Run("Kokoro session mutex poisoned".to_owned()))?;

        let ids_shape: Vec<i64> = vec![1, input_ids.len() as i64];
        let ids_tensor = TensorRef::from_array_view((ids_shape, input_ids))
            .map_err(|e| KokoroInferError::Run(format!("input_ids tensor: {e}")))?;

        let style_shape: Vec<i64> = vec![1, VOICE_STYLE_DIM as i64];
        let style_tensor = TensorRef::from_array_view((style_shape, style))
            .map_err(|e| KokoroInferError::Run(format!("style tensor: {e}")))?;

        let speed_buf = vec![speed];
        let speed_shape: Vec<i64> = vec![1];
        let speed_tensor = TensorRef::from_array_view((speed_shape, speed_buf.as_slice()))
            .map_err(|e| KokoroInferError::Run(format!("speed tensor: {e}")))?;

        let outputs = session
            .run(inputs![
                "input_ids" => ids_tensor,
                "style" => style_tensor,
                "speed" => speed_tensor,
            ])
            .map_err(|e| KokoroInferError::Run(e.to_string()))?;

        let (_, first) = outputs.iter().next().ok_or_else(|| {
            KokoroInferError::OutputShape("Kokoro produced no outputs".to_owned())
        })?;
        let (_, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| KokoroInferError::OutputShape(e.to_string()))?;
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_not_found() {
        let path = PathBuf::from("/no/such/kokoro-model.onnx");
        let result = KokoroSynth::load(&path);
        assert!(matches!(result, Err(KokoroLoadError::NotFound(_))));
    }
}
