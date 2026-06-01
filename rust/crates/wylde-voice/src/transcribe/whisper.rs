//! `ort` session wrapper around the Whisper encoder ONNX.
//!
//! Lifts the working spike configuration from
//! [`rust/spikes/voice-npu-spike/src/main.rs`](../../../../spikes/voice-npu-spike/src/main.rs):
//!
//! * `Session::builder()` with `Level3` optimisation, 4 intra threads
//! * When `backend == Npu`: register `OpenVINO::default()` with
//!   `device_type` + `reshape_input("input_features[1,80,3000]")` +
//!   `with_dynamic_shapes(false)` + cache_dir
//! * Run `inputs!["input_features" => tensor]` against the loaded encoder
//!
//! Errors are mapped onto stable strings so the action layer can
//! translate them into IPC error codes without depending on `ort`'s
//! error types.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::TensorRef;
use ort::inputs;
use thiserror::Error;

use crate::config::SttBackend;
use crate::transcribe::mel::{N_FRAMES, N_MELS};

#[derive(Debug, Error)]
pub enum WhisperLoadError {
    #[error("encoder ONNX not found at {0}")]
    NotFound(PathBuf),

    #[error("ort session build failed: {0}")]
    SessionBuild(String),

    #[error("OpenVINO Execution Provider unavailable at runtime: {0}")]
    OpenVinoUnavailable(String),
}

#[derive(Debug, Error)]
pub enum WhisperInferError {
    #[error("encoder run failed: {0}")]
    Run(String),

    #[error("encoder output missing or wrong dtype: {0}")]
    OutputShape(String),
}

/// A loaded Whisper encoder session, ready to accept one mel input at
/// a time. Wrapped in a mutex because `ort::Session::run` takes `&mut`
/// internally and a single session is shared across all concurrent
/// `voice.transcribe` callers in this slice (one-at-a-time semantics
/// matches the Python `Voice/transcribe.py` lock pattern).
pub struct WhisperEncoder {
    session: Mutex<Session>,
    backend: SttBackend,
    device: String,
    encoder_path: PathBuf,
}

impl WhisperEncoder {
    /// Build a session from the encoder ONNX at `encoder_path`.
    ///
    /// `backend == Cpu` uses ORT's stock CPU EP — works anywhere the
    /// onnxruntime DLL is reachable. `backend == Npu` registers the
    /// OpenVINO EP with `reshape_input` for the static-shape encoder,
    /// per spike findings. NPU load can take ~2.5 s cold; the cache
    /// dir is mandatory for usable repeat-load latency.
    pub fn load(
        encoder_path: &Path,
        backend: SttBackend,
        device_hint: &str,
        ov_cache_dir: &Path,
    ) -> Result<Self, WhisperLoadError> {
        if !encoder_path.exists() {
            return Err(WhisperLoadError::NotFound(encoder_path.to_path_buf()));
        }

        // Early-out for the NPU-without-feature build: refuse before we
        // touch `Session::builder()`, which would try to dlopen
        // onnxruntime.dll and hang on systems that haven't staged the
        // DLL bundle. Same diagnostic the inline `attach_openvino_ep`
        // stub returns — duplicated here so the check fires before any
        // ort APIs are called.
        #[cfg(not(feature = "openvino"))]
        if matches!(backend, SttBackend::Npu) {
            return Err(WhisperLoadError::OpenVinoUnavailable(
                "wylde-voice built without `openvino` cargo feature — rebuild with \
                 `cargo build -p wylde-voice --features openvino` to enable NPU"
                    .to_owned(),
            ));
        }

        let builder = Session::builder()
            .map_err(|e| WhisperLoadError::SessionBuild(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| WhisperLoadError::SessionBuild(e.to_string()))?
            .with_intra_threads(4)
            .map_err(|e| WhisperLoadError::SessionBuild(e.to_string()))?;

        let mut builder = match backend {
            SttBackend::Cpu => builder,
            SttBackend::Npu => attach_openvino_ep(builder, device_hint, ov_cache_dir)?,
        };

        let session = builder
            .commit_from_file(encoder_path)
            .map_err(|e| WhisperLoadError::SessionBuild(e.to_string()))?;

        Ok(Self {
            session: Mutex::new(session),
            backend,
            device: match backend {
                SttBackend::Cpu => "CPU".to_owned(),
                SttBackend::Npu => device_hint.to_owned(),
            },
            encoder_path: encoder_path.to_path_buf(),
        })
    }

    pub fn backend(&self) -> SttBackend {
        self.backend
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn encoder_path(&self) -> &Path {
        &self.encoder_path
    }

    /// Run encoder inference on a precomputed `[N_MELS, N_FRAMES]`
    /// log-mel buffer. Returns the encoder output tensor in
    /// `[batch=1, encoder_seq_len, d_model]` shape — for whisper-tiny.en
    /// that's `[1, 1500, 384]`. The decoder consumes this directly as
    /// its `encoder_hidden_states` input.
    pub fn run_encoder(&self, mel: &[f32]) -> Result<EncoderOutput, WhisperInferError> {
        if mel.len() != N_MELS * N_FRAMES {
            return Err(WhisperInferError::Run(format!(
                "mel buffer wrong size: got {}, want {}",
                mel.len(),
                N_MELS * N_FRAMES
            )));
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| WhisperInferError::Run("encoder mutex poisoned".to_owned()))?;

        let shape: Vec<i64> = vec![1, N_MELS as i64, N_FRAMES as i64];
        let input = TensorRef::from_array_view((shape, mel))
            .map_err(|e| WhisperInferError::Run(format!("tensor build: {e}")))?;

        let outputs = session
            .run(inputs!["input_features" => input])
            .map_err(|e| WhisperInferError::Run(e.to_string()))?;

        let (_, first) = outputs
            .iter()
            .next()
            .ok_or_else(|| WhisperInferError::OutputShape("encoder produced no outputs".to_owned()))?;
        let (out_shape, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| WhisperInferError::OutputShape(e.to_string()))?;

        Ok(EncoderOutput {
            shape: out_shape.to_vec(),
            hidden_states: data.to_vec(),
        })
    }
}

/// Encoder inference result — the `[1, encoder_seq_len, d_model]`
/// hidden-state tensor that the decoder's cross-attention consumes,
/// plus the wire-shape metadata the action layer surfaces in its reply.
#[derive(Debug, Clone)]
pub struct EncoderOutput {
    pub shape: Vec<i64>,
    pub hidden_states: Vec<f32>,
}

/// Attach the OpenVINO Execution Provider to a session builder. Errors
/// at this stage map to `OpenVinoUnavailable` so the action layer can
/// distinguish "DLL bundle missing" (a packaging problem) from "model
/// file missing" (a config / first-run problem).
#[cfg(feature = "openvino")]
fn attach_openvino_ep(
    builder: ort::session::builder::SessionBuilder,
    device_hint: &str,
    ov_cache_dir: &Path,
) -> Result<ort::session::builder::SessionBuilder, WhisperLoadError> {
    use ort::ep::{ArbitrarilyConfigurableExecutionProvider, ExecutionProvider, OpenVINO};

    if !OpenVINO::default()
        .is_available()
        .map_err(|e| WhisperLoadError::OpenVinoUnavailable(e.to_string()))?
    {
        return Err(WhisperLoadError::OpenVinoUnavailable(
            "OpenVINO EP not available at runtime (ORT_DYLIB_PATH / openvino.dll missing?)"
                .to_owned(),
        ));
    }

    if let Err(e) = std::fs::create_dir_all(ov_cache_dir) {
        tracing::warn!(
            "wylde-voice: failed to create OV cache dir {}: {} — load will recompile each time",
            ov_cache_dir.display(),
            e
        );
    }

    let cache_str = ov_cache_dir.to_string_lossy().into_owned();
    let ep = OpenVINO::default()
        .with_device_type(device_hint)
        // Replicates Voice/transcribe.py:278 enc_model.reshape(...) — VPUX
        // can't compile dynamic-shape input_features.
        .with_arbitrary_config("reshape_input", "input_features[1,80,3000]")
        .with_dynamic_shapes(false)
        .with_cache_dir(&cache_str)
        .build()
        .error_on_failure();

    builder
        .with_execution_providers([ep])
        .map_err(|e| WhisperLoadError::SessionBuild(e.to_string()))
}

#[cfg(not(feature = "openvino"))]
fn attach_openvino_ep(
    _builder: ort::session::builder::SessionBuilder,
    _device_hint: &str,
    _ov_cache_dir: &Path,
) -> Result<ort::session::builder::SessionBuilder, WhisperLoadError> {
    Err(WhisperLoadError::OpenVinoUnavailable(
        "wylde-voice built without `openvino` cargo feature — rebuild with \
         `cargo build -p wylde-voice --features openvino` to enable NPU"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_missing_file_returns_not_found() {
        let path = PathBuf::from("/no/such/path/whisper-encoder.onnx");
        let result = WhisperEncoder::load(
            &path,
            SttBackend::Cpu,
            "CPU",
            &PathBuf::from("/tmp/ov_cache"),
        );
        assert!(matches!(result, Err(WhisperLoadError::NotFound(_))));
    }

    #[cfg(not(feature = "openvino"))]
    #[test]
    fn npu_without_feature_returns_unavailable() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // File exists but is empty — should still fail at the EP gate.
        let result = WhisperEncoder::load(
            tmp.path(),
            SttBackend::Npu,
            "NPU",
            &PathBuf::from("/tmp/ov_cache"),
        );
        assert!(matches!(
            result,
            Err(WhisperLoadError::OpenVinoUnavailable(_))
        ));
    }
}
