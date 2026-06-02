//! Kokoro speech-synthesis — phoneme tokeniser, voices.npz loader,
//! Kokoro ONNX session wrapper, and WAV encoder.
//!
//! Slice 11.B wired the deterministic half of TTS: given a phoneme
//! string, look up token ids, slice the per-length style row out of
//! `voices.npz`, run one Kokoro ONNX inference pass, peak-normalise,
//! and pack the float32 PCM into a 16-bit WAV. **Slice 2** (of the
//! voice Rust port) adds the upstream half — text → phonemes — in pure
//! Rust via [`g2p`], so the whole TTS path now runs inside `wylde-voice`
//! with no Python dependency. See [`tokenizer`] for the token-side
//! contract and [`g2p`] for the phonemiser rationale.
//!
//! Submodules:
//!
//! * [`g2p`] — English text → Kokoro IPA phoneme string (misaki-rs,
//!   pure-Rust, no espeak C dep).
//! * [`vocab`] — static Kokoro phoneme → token-id table. Pulled from
//!   `kokoro_onnx/config.json`.
//! * [`tokenizer`] — phoneme string → token id sequence + 0-pad. Slice
//!   11.C adds [`tokenizer::split_phonemes`] for the streaming TTS
//!   sentence-boundary splitter.
//! * [`voices`] — `voices.npz` parser (PKZIP "stored" + .npy v1.0/v2.0
//!   reader, no external deps).
//! * [`kokoro`] — `ort::Session` wrapper. CPU EP only — see module
//!   docstring for why NPU stays deferred.
//! * [`wav`] — float32 PCM → 16-bit PCM WAV + base64 encoder.

pub mod g2p;
pub mod kokoro;
pub mod tokenizer;
pub mod vocab;
pub mod voices;
pub mod wav;

pub use g2p::{british_for_voice, text_to_phonemes};
pub use kokoro::{KokoroInferError, KokoroLoadError, KokoroSynth};
pub use tokenizer::{tokenize, pad_with_zero, split_phonemes, TokenizeResult};
pub use vocab::{KOKORO_SAMPLE_RATE, MAX_PHONEME_LENGTH};
pub use voices::{VoiceStyle, Voices, VoicesLoadError};
pub use wav::{encode_base64, encode_wav, encode_wav_kokoro};
