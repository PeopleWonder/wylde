//! openWakeWord 3-stage ONNX inference pipeline.
//!
//! Each call to [`WakeWordPipeline::process_frame`] consumes exactly
//! `WAKEWORD_FRAME_SAMPLES` (1280) i16 samples and advances:
//! mel → embedding → rolling buffer → classifier. Returns the
//! classifier's scalar score for the current rolling window.
//!
//! The implementation follows openWakeWord's reference export
//! convention (the only published one): single named input, single
//! output tensor per model, embedding model expects `[1, T, 32, 1]`
//! and emits `[1, 96]` per slot.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ort::inputs;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::TensorRef;
use thiserror::Error;

use crate::mic::WAKEWORD_FRAME_SAMPLES;

/// Mel band count emitted by the openWakeWord melspectrogram model.
const MEL_BINS: usize = 32;

/// Embedding dim emitted by the openWakeWord embedding model.
const EMBEDDING_DIM: usize = 96;

/// Rolling window the classifier looks back over. 16 frames × 80 ms ≈
/// 1.28 s — matches the openWakeWord reference timing.
const CLASSIFIER_WINDOW: usize = 16;

/// Default threshold above which a score counts as a detection.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

/// Default cooldown between detections.
pub const DEFAULT_COOLDOWN_MS: u64 = 1_500;

#[derive(Debug, Clone)]
pub struct WakeWordConfig {
    pub mel_model_path: PathBuf,
    pub embedding_model_path: PathBuf,
    pub classifier_model_path: PathBuf,
    pub threshold: f32,
    pub cooldown_ms: u64,
}

impl WakeWordConfig {
    /// Default per-model layout inside `<base_dir>/<model_name>/`:
    ///
    /// * `melspectrogram.onnx`
    /// * `embedding_model.onnx`
    /// * `<model_name>.onnx`
    pub fn from_layout(base_dir: &Path, model_name: &str) -> Self {
        let dir = base_dir.join(model_name);
        Self {
            mel_model_path: dir.join("melspectrogram.onnx"),
            embedding_model_path: dir.join("embedding_model.onnx"),
            classifier_model_path: dir.join(format!("{model_name}.onnx")),
            threshold: DEFAULT_THRESHOLD,
            cooldown_ms: DEFAULT_COOLDOWN_MS,
        }
    }
}

#[derive(Debug, Error)]
pub enum WakeWordLoadError {
    #[error("wake-word model file missing: {0}")]
    MissingFile(PathBuf),

    #[error("ort session build failed for {0}: {1}")]
    SessionBuild(PathBuf, String),
}

#[derive(Debug, Error)]
pub enum WakeWordInferError {
    #[error("frame size must be {WAKEWORD_FRAME_SAMPLES}, got {0}")]
    WrongFrameSize(usize),

    #[error("mel session run failed: {0}")]
    MelRun(String),

    #[error("embedding session run failed: {0}")]
    EmbeddingRun(String),

    #[error("classifier session run failed: {0}")]
    ClassifierRun(String),

    #[error("model output shape unexpected: {0}")]
    OutputShape(String),

    #[error("internal lock poisoned: {0}")]
    LockPoisoned(String),
}

pub struct WakeWordPipeline {
    mel: Mutex<Session>,
    embedding: Mutex<Session>,
    classifier: Mutex<Session>,
    rolling: Mutex<VecDeque<[f32; EMBEDDING_DIM]>>,
    config: WakeWordConfig,
    mel_input_name: String,
    embedding_input_name: String,
    classifier_input_name: String,
}

impl WakeWordPipeline {
    /// Load every model file. Returns [`WakeWordLoadError::MissingFile`]
    /// for the first missing path so the action layer can surface a
    /// stable `model_not_loaded` to the caller before any DLL work
    /// happens.
    pub fn load(config: WakeWordConfig) -> Result<Self, WakeWordLoadError> {
        for path in [
            &config.mel_model_path,
            &config.embedding_model_path,
            &config.classifier_model_path,
        ] {
            if !path.exists() {
                return Err(WakeWordLoadError::MissingFile(path.clone()));
            }
        }

        let mel = build_session(&config.mel_model_path)?;
        let embedding = build_session(&config.embedding_model_path)?;
        let classifier = build_session(&config.classifier_model_path)?;

        let mel_input_name = first_input_name(&mel);
        let embedding_input_name = first_input_name(&embedding);
        let classifier_input_name = first_input_name(&classifier);

        Ok(Self {
            mel: Mutex::new(mel),
            embedding: Mutex::new(embedding),
            classifier: Mutex::new(classifier),
            rolling: Mutex::new(VecDeque::with_capacity(CLASSIFIER_WINDOW)),
            config,
            mel_input_name,
            embedding_input_name,
            classifier_input_name,
        })
    }

    pub fn config(&self) -> &WakeWordConfig {
        &self.config
    }

    /// Run mel → embedding → classifier on a single 1280-sample frame.
    /// Returns the classifier's score for the current rolling window,
    /// or `None` while the window is still warming up (< 16 embeddings).
    pub fn process_frame(&self, samples: &[i16]) -> Result<Option<f32>, WakeWordInferError> {
        if samples.len() != WAKEWORD_FRAME_SAMPLES {
            return Err(WakeWordInferError::WrongFrameSize(samples.len()));
        }

        let pcm_f32: Vec<f32> = samples.iter().map(|&s| s as f32).collect();
        let mel_frames = self.run_mel(&pcm_f32)?;
        for frame in mel_frames {
            let embedding = self.run_embedding(&frame)?;
            self.push_embedding(embedding)?;
        }

        let rolling = self
            .rolling
            .lock()
            .map_err(|e| WakeWordInferError::LockPoisoned(format!("rolling: {e}")))?;
        if rolling.len() < CLASSIFIER_WINDOW {
            return Ok(None);
        }
        let stack: Vec<f32> = rolling
            .iter()
            .flat_map(|e| e.iter().copied())
            .collect();
        drop(rolling);

        let score = self.run_classifier(&stack)?;
        Ok(Some(score))
    }

    pub fn threshold(&self) -> f32 {
        self.config.threshold
    }

    pub fn cooldown_ms(&self) -> u64 {
        self.config.cooldown_ms
    }

    fn run_mel(&self, samples: &[f32]) -> Result<Vec<[f32; MEL_BINS]>, WakeWordInferError> {
        let mut sess = self
            .mel
            .lock()
            .map_err(|e| WakeWordInferError::LockPoisoned(format!("mel: {e}")))?;
        let shape: Vec<i64> = vec![1, samples.len() as i64];
        let input = TensorRef::from_array_view((shape, samples))
            .map_err(|e| WakeWordInferError::MelRun(format!("tensor build: {e}")))?;
        let outputs = sess
            .run(inputs![self.mel_input_name.as_str() => input])
            .map_err(|e| WakeWordInferError::MelRun(e.to_string()))?;
        let (_, first) = outputs
            .iter()
            .next()
            .ok_or_else(|| WakeWordInferError::OutputShape("mel produced no outputs".to_owned()))?;
        let (out_shape, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| WakeWordInferError::OutputShape(format!("mel: {e}")))?;
        // Accept [1, 1, T, 32], [1, T, 32], or [T, 32]; collapse to the
        // last two axes and slice into per-T `[32]` rows.
        let bins = *out_shape
            .last()
            .ok_or_else(|| WakeWordInferError::OutputShape("mel shape rank 0".to_owned()))?;
        if bins as usize != MEL_BINS {
            return Err(WakeWordInferError::OutputShape(format!(
                "mel last dim = {bins}, want {MEL_BINS}"
            )));
        }
        let total: i64 = out_shape.iter().product();
        if data.len() as i64 != total {
            return Err(WakeWordInferError::OutputShape(format!(
                "mel data len {} != product(shape) {total}",
                data.len()
            )));
        }
        let frames = data.len() / MEL_BINS;
        let mut out = Vec::with_capacity(frames);
        for i in 0..frames {
            let mut row = [0.0_f32; MEL_BINS];
            row.copy_from_slice(&data[i * MEL_BINS..(i + 1) * MEL_BINS]);
            out.push(row);
        }
        Ok(out)
    }

    fn run_embedding(
        &self,
        mel_frame: &[f32; MEL_BINS],
    ) -> Result<[f32; EMBEDDING_DIM], WakeWordInferError> {
        let mut sess = self
            .embedding
            .lock()
            .map_err(|e| WakeWordInferError::LockPoisoned(format!("embedding: {e}")))?;
        // openWakeWord embedding expects [1, 76, 32, 1] for 16-frame
        // contexts; for the per-frame slot we pass [1, 1, 32, 1].
        let shape: Vec<i64> = vec![1, 1, MEL_BINS as i64, 1];
        let input = TensorRef::from_array_view((shape, mel_frame.as_slice()))
            .map_err(|e| WakeWordInferError::EmbeddingRun(format!("tensor build: {e}")))?;
        let outputs = sess
            .run(inputs![self.embedding_input_name.as_str() => input])
            .map_err(|e| WakeWordInferError::EmbeddingRun(e.to_string()))?;
        let (_, first) = outputs.iter().next().ok_or_else(|| {
            WakeWordInferError::OutputShape("embedding produced no outputs".to_owned())
        })?;
        let (out_shape, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| WakeWordInferError::OutputShape(format!("embedding: {e}")))?;
        let last = *out_shape.last().ok_or_else(|| {
            WakeWordInferError::OutputShape("embedding shape rank 0".to_owned())
        })?;
        if last as usize != EMBEDDING_DIM {
            return Err(WakeWordInferError::OutputShape(format!(
                "embedding last dim = {last}, want {EMBEDDING_DIM}"
            )));
        }
        if data.len() < EMBEDDING_DIM {
            return Err(WakeWordInferError::OutputShape(format!(
                "embedding data len {} < {EMBEDDING_DIM}",
                data.len()
            )));
        }
        let mut out = [0.0_f32; EMBEDDING_DIM];
        out.copy_from_slice(&data[data.len() - EMBEDDING_DIM..]);
        Ok(out)
    }

    fn push_embedding(
        &self,
        embedding: [f32; EMBEDDING_DIM],
    ) -> Result<(), WakeWordInferError> {
        let mut rolling = self
            .rolling
            .lock()
            .map_err(|e| WakeWordInferError::LockPoisoned(format!("rolling: {e}")))?;
        if rolling.len() == CLASSIFIER_WINDOW {
            let _ = rolling.pop_front();
        }
        rolling.push_back(embedding);
        Ok(())
    }

    fn run_classifier(&self, flat: &[f32]) -> Result<f32, WakeWordInferError> {
        let mut sess = self
            .classifier
            .lock()
            .map_err(|e| WakeWordInferError::LockPoisoned(format!("classifier: {e}")))?;
        let shape: Vec<i64> = vec![1, CLASSIFIER_WINDOW as i64, EMBEDDING_DIM as i64];
        let input = TensorRef::from_array_view((shape, flat))
            .map_err(|e| WakeWordInferError::ClassifierRun(format!("tensor build: {e}")))?;
        let outputs = sess
            .run(inputs![self.classifier_input_name.as_str() => input])
            .map_err(|e| WakeWordInferError::ClassifierRun(e.to_string()))?;
        let (_, first) = outputs.iter().next().ok_or_else(|| {
            WakeWordInferError::OutputShape("classifier produced no outputs".to_owned())
        })?;
        let (_, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| WakeWordInferError::OutputShape(format!("classifier: {e}")))?;
        data.first().copied().ok_or_else(|| {
            WakeWordInferError::OutputShape("classifier emitted empty tensor".to_owned())
        })
    }
}

fn build_session(path: &Path) -> Result<Session, WakeWordLoadError> {
    Session::builder()
        .map_err(|e| WakeWordLoadError::SessionBuild(path.to_path_buf(), e.to_string()))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| WakeWordLoadError::SessionBuild(path.to_path_buf(), e.to_string()))?
        .with_intra_threads(2)
        .map_err(|e| WakeWordLoadError::SessionBuild(path.to_path_buf(), e.to_string()))?
        .commit_from_file(path)
        .map_err(|e| WakeWordLoadError::SessionBuild(path.to_path_buf(), e.to_string()))
}

fn first_input_name(session: &Session) -> String {
    session
        .inputs()
        .first()
        .map(|i| i.name().to_owned())
        .unwrap_or_else(|| "input".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn config_layout_resolves_canonical_paths() {
        let base = PathBuf::from("/tmp/wylde-wakeword-test");
        let cfg = WakeWordConfig::from_layout(&base, "hey-jarvis");
        assert_eq!(
            cfg.mel_model_path,
            base.join("hey-jarvis").join("melspectrogram.onnx")
        );
        assert_eq!(
            cfg.embedding_model_path,
            base.join("hey-jarvis").join("embedding_model.onnx")
        );
        assert_eq!(
            cfg.classifier_model_path,
            base.join("hey-jarvis").join("hey-jarvis.onnx")
        );
        assert!((cfg.threshold - DEFAULT_THRESHOLD).abs() < f32::EPSILON);
        assert_eq!(cfg.cooldown_ms, DEFAULT_COOLDOWN_MS);
    }

    #[test]
    fn load_missing_mel_returns_missing_file() {
        let tmp = tempdir().unwrap();
        let cfg = WakeWordConfig::from_layout(tmp.path(), "missing-model");
        match WakeWordPipeline::load(cfg.clone()) {
            Ok(_) => panic!("expected MissingFile load error, got Ok(...)"),
            Err(WakeWordLoadError::MissingFile(p)) => {
                assert_eq!(p, cfg.mel_model_path);
            }
            Err(other) => panic!("expected MissingFile, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_embedding_returns_missing_file_for_embedding() {
        let tmp = tempdir().unwrap();
        let model_name = "fake";
        let model_dir = tmp.path().join(model_name);
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("melspectrogram.onnx"), b"stub").unwrap();
        // embedding_model.onnx + classifier file still missing.
        let cfg = WakeWordConfig::from_layout(tmp.path(), model_name);
        match WakeWordPipeline::load(cfg.clone()) {
            Ok(_) => panic!("expected MissingFile load error, got Ok(...)"),
            Err(WakeWordLoadError::MissingFile(p)) => {
                assert_eq!(p, cfg.embedding_model_path);
            }
            Err(other) => panic!("expected MissingFile, got {other:?}"),
        }
    }
}
