//! Wake-word detection (Slice 11.D).
//!
//! openWakeWord-style 3-stage ONNX pipeline driven from a dedicated
//! listener thread that pulls 80 ms frames off the shared `mic`
//! capture and emits detection events on a broadcast channel.
//!
//! ## Model architecture
//!
//! openWakeWord splits the per-frame pipeline into three ONNX models:
//!
//! 1. **Melspectrogram** — 1280 samples (80 ms @ 16 kHz) → `[T, 32]`
//!    log-mel frames (T varies with the upstream export, typically 5).
//! 2. **Embedding** — `[T, 32, 1]` mel chunk → `[1, 96]` embedding.
//!    A rolling buffer of the last 16 embeddings becomes the
//!    classifier input.
//! 3. **Wake-word classifier** — `[1, 16, 96]` rolling embedding stack
//!    → `[1, 1]` score in [0, 1]. Per-model trained; the default we
//!    target is `openWakeWord/hey-jarvis` (mirror of Python
//!    `Voice/state.py:DEFAULT_WAKE_WORD_MODEL`).
//!
//! ## Why the load is best-effort
//!
//! The ONNX bundles aren't yet shipped in the HuggingFace cache by
//! first-run setup — that work lands when Phase 8's model registry
//! gains a `kind: "wakeword"` field. Until then, `WakeWordPipeline::load`
//! returns `model_not_loaded` cleanly and the `voice.wakeword.start`
//! action surfaces that to the caller without crashing the service.
//!
//! ## Cooldown
//!
//! Per-detection cooldown (default 1.5 s) suppresses repeat fires
//! while the user is mid-utterance. Same shape Python's
//! [`Voice/wake_word.py`](../../../../../Voice/wake_word.py) intended
//! before the implementation was stubbed.

pub mod download;
pub mod listener;
pub mod pipeline;

pub use listener::{WakeWordEvent, WakeWordListener};
pub use pipeline::{
    WakeWordConfig, WakeWordInferError, WakeWordLoadError, WakeWordPipeline,
};
