//! Whisper speech-to-text — audio decode, log-mel preprocess, ONNX
//! encoder + decoder inference, tokenizer integration.
//!
//! Slice 11.A+ now wires the end-to-end pipeline: WAV → mel → encoder →
//! decoder loop (greedy sampling) → tokenizer decode → text.
//!
//! Submodules:
//!
//! * [`audio`] — WAV decode to f32 mono 16 kHz PCM (`hound`-based;
//!   resamples and downmixes when needed).
//! * [`mel`] — log-mel spectrogram preprocessing — Hann-windowed STFT
//!   + Slaney mel filterbank + log10 + dynamic-range clip. Produces the
//!     `[1, 80, 3000]` tensor Whisper's encoder expects.
//! * [`whisper`] — `ort` session wrapper for the encoder. Loads with the
//!   OpenVINO EP when `stt.backend == npu` (static `[1, 80, 3000]`
//!   reshape, matching the spike's findings); returns the
//!   `[1, 1500, 384]` hidden-state tensor for the decoder.
//! * [`decoder`] — `ort` session wrapper for the no-past Whisper
//!   decoder, run on the CPU EP. Greedy autoregressive sampling with
//!   an EOT stop or `max_new_tokens` cap. KV-cached variant + NPU
//!   decoder deferred (see module docstring).
//! * [`tokenizer`] — Hugging Face `tokenizers` wrapper. Loads the
//!   `tokenizer.json` from the snapshot, builds the seeded decoder
//!   prompt, and detokenises generated IDs back to text.
//!
//! Slice 11.C adds streaming output via
//! [`crate::actions::transcribe::handle_transcribe_stream`] —
//! [`decoder::WhisperDecoder::generate_with_callback`] is the lower-level
//! hook the streaming handler drives.
//!
//! Deferred to later slices:
//! * Slice 11.D — wake-word + `cpal`-based microphone capture (including
//!   bidirectional chunked-audio input).
//! * Slice 11.E — flip `WYLDE_WYLDE_VOICE_IMPL` default from Python to Rust.

pub mod audio;
pub mod decoder;
pub mod mel;
pub mod tokenizer;
pub mod whisper;

pub use decoder::{DecoderLoadError, WhisperDecoder};
pub use tokenizer::{TokenizerLoadError, WhisperTokenizer};
pub use whisper::{EncoderOutput, WhisperEncoder, WhisperLoadError};
